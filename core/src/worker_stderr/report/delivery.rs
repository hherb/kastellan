//! Whether a report actually **reaches** someone, and whether writing it can
//! kill the process.
//!
//! Two questions that the three emitters in `report`'s sibling modules
//! (`tool_worker`, `persistent`) all have to answer the same way — none of them
//! lives here, which is the entire point: see [`warn_and_fall_back`] for why
//! the check must expand at *their* callsites and not in this module. They used
//! to be answered by one line —
//! `if !tracing::dispatcher::has_been_set() { eprintln!(…) }`. That line was
//! wrong in both halves:
//!
//! * it asked whether a subscriber **exists**, not whether *this* event would
//!   be **recorded** ([#734](https://github.com/hherb/kastellan/issues/734)), and
//! * `eprintln!` **panics** when the write fails, which under the workspace
//!   release profile (`panic = "abort"`) is a silent `SIGABRT`
//!   ([#733](https://github.com/hherb/kastellan/issues/733)).
//!
//! The two are one change because fixing the first *creates* the second: before
//! #734 the daemon always took the `tracing` branch, so no deployed daemon
//! could ever reach the `eprintln!`. After it, a daemon whose operator narrowed
//! `RUST_LOG` falls through to the fallback like any test binary does.

use std::os::fd::RawFd;

/// Would a write to `fd` fail because nothing can receive it any more?
///
/// Returns `false` only when the kernel says the descriptor is **unusable** —
/// the reader of a pipe has gone away, or the descriptor was never open. Every
/// other answer, including "cannot tell", is `true`.
///
/// ## Why this exists rather than just writing and checking the error
///
/// The fallback has to go out through `eprintln!` and not
/// `writeln!(std::io::stderr(), …)`, because libtest captures through
/// `std::io::set_output_capture`, which the `print!`/`eprint!` **macros**
/// consult and the `Stdout`/`Stderr` handles do not — a report written through
/// the handle never appears beneath the failing test that needs it. But
/// `eprintln!` has no fallible form: on a write error it panics, and
/// `panic = "abort"` turns that into `SIGABRT` with no message. So the only
/// place left to be careful is *before* the macro, which is what this is.
///
/// ## Why `poll` and not a trial write
///
/// A zero-length `write(2)` to a pipe with no reader returns `0`, not `EPIPE`,
/// so it detects nothing; a one-byte trial write detects it but puts a stray
/// byte in every healthy log. `poll` with a zero timeout answers without
/// writing anything at all.
///
/// ## The three bits, and why all three
///
/// ⚠️ **The two pipe bits are host-specific, and the split is measured, not
/// assumed.** `a_pipe_whose_reader_is_gone_is_not_writable` was run against a
/// mask with each bit deleted in turn, on both hosts:
///
/// | mutant | macOS (arm64) | Linux (aarch64) |
/// | --- | --- | --- |
/// | drop `POLLERR` | **SURVIVED** | KILLED |
/// | drop `POLLHUP` | KILLED | **SURVIVED** |
///
/// So macOS reports a broken pipe write-end as `POLLHUP` and Linux as
/// `POLLERR`, each bit is load-bearing on exactly one host, and **neither host
/// alone can prove this mask**. A reviewer who sees the surviving mutant on one
/// machine and deletes the "dead" bit breaks the other platform silently —
/// which is the cross-platform rule in CLAUDE.md with a concrete price tag.
///
/// * `POLLERR` / `POLLHUP` — the write end of a pipe whose read end has closed.
///   In both cases a real `write_all` on that same descriptor returns
///   `BrokenPipe`, which the test asserts as ground truth before believing the
///   probe.
/// * `POLLNVAL` — **corroborated with `fcntl(F_GETFD)`**, the descriptor is
///   not open at all, which is what `2>&-` leaves behind.
///
/// ⚠️ **Only the pipe bits prevent an abort; the `POLLNVAL` arm prevents a
/// pointless write.** Measured: `std` swallows `EBADF` on stdio
/// (`handle_ebadf` in `library/std/src/io/stdio.rs`), so `eprintln!` to a
/// **closed** fd 2 returns `Ok` and does **not** panic — only `EPIPE` does.
///
/// | stderr shape | `std::io::stderr().write` | `eprintln!` |
/// | --- | --- | --- |
/// | closed (`2>&-`) | `Ok(1)` — swallowed | does **not** panic |
/// | pipe, reader gone | `Err(BrokenPipe)` | **panics** |
///
/// So #733's `SIGABRT` is reachable through the broken-pipe shape only — the
/// one the issue names. That does not make the `POLLNVAL` arm optional, but it
/// does change what it is for, and it is why getting it wrong cost *reports*
/// rather than *crashes* on macOS. Both shapes are driven end to end by
/// `core/tests/worker_report_broken_stderr_e2e.rs`.
///
/// ⚠️ **`POLLNVAL` alone does NOT mean "not open" on macOS, and reading it
/// that way was a fail-CLOSED bug.** Darwin's `poll` reports `POLLNVAL` for
/// perfectly usable **character devices**, which the bare mask then read as
/// unusable. Measured on macOS 27 (arm64), `events = POLLOUT`, zero timeout:
///
/// | fd | `revents` | bare mask said | a real `write` |
/// | --- | --- | --- | --- |
/// | `/dev/null`, `O_WRONLY` | `0x20` `POLLNVAL` | **unwritable** | **succeeds** |
/// | genuinely closed fd | `0x20` `POLLNVAL` | unwritable | `EBADF` |
///
/// So any process run `2>/dev/null` — a CLI invocation, a developer's
/// `cargo test … 2>/dev/null` — suppressed **every** worker report on macOS,
/// which is the one direction this module's own doc forbids. Linux does not
/// share the quirk, so this is the third bit's version of the `POLLERR` /
/// `POLLHUP` split below: one host cannot prove it. `fcntl(F_GETFD)` answers
/// "is this open" authoritatively and portably, so `POLLNVAL` is now believed
/// only when `fcntl` agrees. Pinned by
/// [`a_character_device_is_writable`] and
/// [`a_descriptor_that_was_never_open_is_not_writable`], which must both stay.
///
/// ⚠️ **This is a race, and it is meant to be one.** The reader can close
/// between the probe and the write. The probe removes the *reproducible*
/// abort — a pipeline whose reader has already exited, which is
/// `kastellan-cli guard capture … | head` — not every possible one. There is
/// no way to close the window while `eprintln!` is the required macro, and
/// narrowing a reproducible crash to an unreproducible one is worth doing
/// even though it is not a proof.
///
/// ⚠️ **"Cannot tell" means `true`, deliberately.** A `poll` that fails, or
/// that reports the descriptor merely not-ready-for-writing (a full pipe with
/// a live reader), must not suppress the report — suppressing it is exactly
/// the silence this whole module exists to remove. The conservative direction
/// here is to *attempt* the write.
pub(super) fn is_writable(fd: RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // Zero timeout: this must never block. A `poll` on a descriptor that is
    // simply not ready returns 0 immediately with no bits set.
    let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
    if ready < 0 {
        // `poll` itself failed (EINTR, or a bad `nfds`). We learned nothing, so
        // do not suppress the report.
        return true;
    }
    // A broken pipe/socket: conclusive on both hosts, one bit per host.
    if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
        return false;
    }
    // `POLLNVAL` only counts when `fcntl` agrees the descriptor is not open —
    // on macOS it also fires for live character devices. See this function's
    // doc for the measured table.
    if pfd.revents & libc::POLLNVAL != 0 && !is_open(fd) {
        return false;
    }
    true
}

/// Is `fd` an open descriptor at all?
///
/// `fcntl(F_GETFD)` is the portable, authoritative answer — it touches no
/// data, cannot block, and returns `-1`/`EBADF` for exactly the `2>&-` case.
/// It exists as its own function so [`is_writable`]'s corroboration step is
/// nameable from a test.
fn is_open(fd: RawFd) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0
}

/// Is this process's own stderr writable? The [`is_writable`] probe, exported
/// for `kastellan-tests-common`'s panic hook
/// ([#749](https://github.com/hherb/kastellan/issues/749)).
///
/// That hook renders a panic with `eprintln!` for the same libtest-capture
/// reason this module does, and inherits the same #733 hazard — worse, in
/// fact: an `eprintln!` that panics *inside a panic hook* is a panic while
/// panicking, which aborts immediately with **no output at all**. Measured on
/// this Mac: unguarded, signal 6 and nothing on any stream; guarded, a clean
/// exit 101 with libtest still reporting the failure itself.
///
/// ⚠️ **What the guard saves is the ACCOUNT of the failure, not the panic
/// text.** On a broken fd 2 the message is exactly what cannot be written, and
/// the hook drops it — the guard's whole job. What survives is the process, and
/// with it libtest's own `test result: FAILED` line, which a SIGABRT destroys.
/// That is the difference from the #733 case this borrows the probe from:
/// there a report went missing, here the entire account of the failure would.
///
/// ⚠️ **Takes no `fd`, deliberately.** The one mutant #745's review found
/// surviving even `-D warnings` was probing `STDOUT_FILENO` instead of
/// `STDERR_FILENO` — stdout is writable in every test binary, so nothing
/// catches it. A parameterless export cannot be called wrongly that way.
///
/// ⚠️ **`#[doc(hidden)]`, deliberately**, for the same reason
/// [`crate::untrusted_text`] is: `kastellan-core` is published, and a bare
/// `pub` here would be a permanent semver commitment taken on for one
/// dev-dependency's benefit. The alternative — hand-copying [`is_writable`]
/// and its `is_open` helper into `tests-common`, with the per-host `POLLERR` /
/// `POLLHUP` / `POLLNVAL` reasoning that goes with them — is the
/// drift-between-copies shape CLAUDE.md's
/// bwrap-argv note names, and this probe has already needed one host-specific
/// correction (`POLLNVAL` on macOS character devices) that a copy would not
/// have received.
#[doc(hidden)]
pub fn stderr_is_writable() -> bool {
    is_writable(libc::STDERR_FILENO)
}

/// Write `line` to the process's own stderr, unless stderr is already broken.
///
/// The one place `eprintln!` is called for a worker report. Returns whether the
/// line was attempted, which the tests use as the observable — a `bool` the
/// caller ignores in production but which makes "we deliberately said nothing"
/// distinguishable from "we said something" without reading a stream.
///
/// ⚠️ **`eprintln!` is load-bearing and must stay** — see [`is_writable`] for
/// the libtest-capture argument. Do not "simplify" this to
/// `writeln!(std::io::stderr(), …)`; `core/tests/worker_early_exit_stderr_fallback_e2e.rs`
/// asserts end to end that the report lands in a *failing test's* captured
/// block, which the handle form would break.
pub(super) fn write_fallback_line(line: &str) -> bool {
    if !is_writable(libc::STDERR_FILENO) {
        return false;
    }
    eprintln!("{line}");
    true
}

/// Emit `$line` on whichever channel will actually carry it, deciding from the
/// **caller's** module.
///
/// The two channels are mutually exclusive by construction — `tracing` when it
/// will record the event, the marked stderr line when it will not — so a
/// reader never sees the same report twice. Pinned by
/// [`a_recorded_event_does_not_also_fall_back`].
///
/// Evaluates to `true` when the stderr fallback line was written — i.e. when
/// `tracing` was **not** going to record the event. Production call sites
/// discard it with a `;`. It exists because the tests below have to observe
/// which branch was taken, and they cannot do that by reading output: libtest
/// captures a passing test's `eprintln!` and gives it back to no one.
///
/// ## Why a macro and not a function
///
/// This is the one place in the tree where a macro is the correct tool rather
/// than a shortcut, and the reason is measurable. The delivery check
/// (`tracing::event_enabled!`) creates its **own callsite**, carrying the
/// `module_path!` and fields of wherever it is written. `EnvFilter` matches on
/// exactly those. So a check written in a shared *function* answers for the
/// shared function's target, not for the `warn!` it claims to be speaking for.
///
/// Measured against `tracing-subscriber` 0.3.23, with `emitter` holding the
/// `warn!` and `shared` holding the check:
///
/// | directive | check in `emitter` | check in `shared` | actually recorded |
/// | --- | --- | --- | --- |
/// | `…::emitter=warn` | true | **false** | true |
/// | `…::shared=warn` | false | **true** | **false** |
/// | `warn` | true | true | true |
///
/// Row two is the one that matters: a check living in a shared module reports
/// "delivered" for an event that was **not** recorded, and the fallback stays
/// quiet — which is #734 reproduced *inside its own fix*. Expanding at the
/// caller makes the two callsites share a module, and therefore a target, by
/// construction rather than by whoever adds the next emitter remembering to.
///
/// ## Why the fields are repeated in the check
///
/// `EnvFilter` directives can carry field predicates, and a field-carrying
/// event is matched by a *different* set of them than a bare one. Measured,
/// same harness, with the `warn!` carrying `%label`:
///
/// | directive | bare check | check with `label` | actually recorded |
/// | --- | --- | --- | --- |
/// | `warn,…[{label}]=off` | **true** | false | **false** |
/// | `…[{label}]=warn` | false | true | true |
///
/// The bare check is fail-**open** in row one. Declaring the same fields on the
/// `event_enabled!` made the check track `recorded` exactly across all seven
/// directives tried, which is why the two arms below exist rather than one.
///
/// ⚠️ **`message` counts as a field, and forgetting it reopened #734 in the
/// first version of this macro.** A `warn!("{line}")` always carries an
/// implicit `message` field; the checks originally declared `label` and not
/// `message`, so **both** arms went silent on both channels under a
/// `[{message}]` predicate — the same hole the `label` arm exists to close,
/// one field further along. Measured on the same harness:
///
/// | directive | check without `message` | check with `message` | actually recorded |
/// | --- | --- | --- | --- |
/// | `warn,…[{message}]=off` (bare arm) | **true** | false | **false** |
/// | `warn,…[{message}]=off` (labelled arm) | **true** | false | **false** |
///
/// With `message` declared, `fell_back` tracks `recorded` exactly across all
/// seven directives on **both** arms. The rule this keeps proving: **every
/// field the `warn!` carries must be named in the check**, implicit ones
/// included. Pinned by [`a_message_scoped_off_directive_fools_neither_arm`].
///
/// ⚠️ **The two callsites are still not the same callsite** — they differ in
/// line number, and nothing stops a future `EnvFilter` from discriminating on
/// that. They agree on everything `EnvFilter` matches on today (target, level,
/// field names). If that ever stops being true the failure is a *duplicated*
/// report, not a suppressed one, in every case except a directive that enables
/// the check's line and not the `warn!`'s — which no directive syntax can
/// currently express.
macro_rules! warn_and_fall_back {
    // No structured fields: the tool-worker failure report.
    ($marker:expr, $line:expr $(,)?) => {{
        let line: &str = $line;
        ::tracing::warn!("{line}");
        // Expands HERE, in the caller's module, for the reasons in this
        // macro's doc. Moving it into a helper function silently reintroduces
        // #734.
        if !::tracing::event_enabled!(::tracing::Level::WARN, message) {
            $crate::worker_stderr::report::delivery::write_fallback_line(
                &$crate::worker_stderr::report::shared::format_stderr_fallback($marker, line),
            )
        } else {
            false
        }
    }};
    // One `label` field: both persistent-worker reports. The field is named in
    // the check as well as in the `warn!`, which is the whole point of having a
    // second arm rather than falling through to the first.
    ($marker:expr, $line:expr, label = $label:expr $(,)?) => {{
        let line: &str = $line;
        let label: &str = $label;
        ::tracing::warn!(%label, "{line}");
        if !::tracing::event_enabled!(::tracing::Level::WARN, label, message) {
            $crate::worker_stderr::report::delivery::write_fallback_line(
                &$crate::worker_stderr::report::shared::format_stderr_fallback($marker, line),
            )
        } else {
            false
        }
    }};
}

pub(super) use warn_and_fall_back;

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::super::shared::STDERR_FALLBACK_MARKERS;
    use super::*;

    /// A pipe, as a pair of raw descriptors we own and close by hand.
    ///
    /// Deliberately not `std::process::Command`'s plumbing or a `File`: the
    /// point is to control exactly when the read end closes, which is the
    /// state [`is_writable`] has to recognise.
    struct Pipe {
        read: RawFd,
        write: RawFd,
    }

    impl Pipe {
        fn new() -> Self {
            let mut fds = [0 as RawFd; 2];
            assert_eq!(
                unsafe { libc::pipe(fds.as_mut_ptr()) },
                0,
                "the test needs a pipe; this is a host problem, not a code one"
            );
            Self {
                read: fds[0],
                write: fds[1],
            }
        }

        /// Close the read end, which is what makes the write end broken.
        fn close_read_end(&mut self) {
            if self.read >= 0 {
                unsafe { libc::close(self.read) };
                self.read = -1;
            }
        }

        /// Ground truth: does a real write to the write end actually fail?
        ///
        /// Without this the suite would be asserting that `is_writable` agrees
        /// with our *belief* about the pipe's state rather than with the
        /// kernel's behaviour — and a probe that returned `false` for every
        /// descriptor would pass the broken-pipe test while silently disabling
        /// the fallback everywhere.
        fn a_real_write_fails(&self) -> bool {
            let mut f =
                unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(self.write) };
            let failed = f.write_all(b"x").is_err();
            // We do not own this descriptor through the `File`; `Drop` would
            // close it out from under `self`.
            std::mem::forget(f);
            failed
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            self.close_read_end();
            if self.write >= 0 {
                unsafe { libc::close(self.write) };
            }
        }
    }


    /// A stand-in emitter living in its **own module**, exactly as the three
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
        let (fell_back, recorded) =
            under(&directive, || pretend_emitter::emit_with_label(PROBE_LINE, "matrix"));
        assert!(
            !recorded,
            "POSITIVE CONTROL: a field-scoped `off` must drop the labelled warn, or this test \
             is not exercising the disagreement it exists for. Directive: {directive}"
        );
        assert!(
            fell_back,
            "the labelled arm's check must declare the same `label` field its `warn!` does. A \
             bare `event_enabled!(Level::WARN)` answers `true` under `{directive}` while the \
             event is dropped — silence, which is the failure mode this module exists to \
             remove."
        );
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
        let directive = format!("warn,{}[{{message}}]=off", pretend_emitter::target());
        for (arm, fired) in [
            ("bare", Box::new(|| pretend_emitter::emit(PROBE_LINE)) as Box<dyn Fn() -> bool>),
            (
                "labelled",
                Box::new(|| pretend_emitter::emit_with_label(PROBE_LINE, "matrix")),
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
    /// ⚠️ **A new emitter must join this array.** Nothing forces it — a fourth
    /// `emit_*` that called a plain function instead of `warn_and_fall_back!`
    /// would be silently un-guarded, which is the whole failure this array
    /// exists to prevent. [`every_shipping_emitter_is_covered`] is the only
    /// thing that notices, and it can only count what it is given.
    #[allow(clippy::type_complexity)]
    fn shipping_emitters() -> Vec<(&'static str, &'static str, Box<dyn Fn() -> bool>)> {
        const TOOL_WORKER: &str = "kastellan_core::worker_stderr::report::tool_worker";
        const PERSISTENT: &str = "kastellan_core::worker_stderr::report::persistent";
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
        ]
    }

    #[test]
    fn every_shipping_emitter_checks_delivery_at_its_own_callsite() {
        // The blind spot this closes: every other test in this file drives
        // `pretend_emitter`, which proves the MACRO works and nothing about the
        // three functions that actually ship. A refactor replacing the macro in
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
            "there is exactly one shipping emitter per fallback marker; if a fourth marker was \
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

    #[test]
    fn a_live_pipe_is_writable() {
        // POSITIVE CONTROL for the whole file. Without it, an `is_writable`
        // that returned `false` unconditionally would pass every other test
        // here — while suppressing every worker report in the tree, which is
        // the exact silence this module exists to remove.
        let p = Pipe::new();
        assert!(
            is_writable(p.write),
            "a pipe with its read end still open must be reported writable, or the fallback \
             is suppressed for every healthy process"
        );
        assert!(
            !p.a_real_write_fails(),
            "ground truth: a real write to a live pipe must succeed, or this fixture is not \
             testing what it claims"
        );
    }

    #[test]
    fn a_pipe_whose_reader_is_gone_is_not_writable() {
        let mut p = Pipe::new();
        p.close_read_end();
        // Ground truth FIRST, so a fixture that failed to break the pipe is a
        // red here rather than a silently vacuous assertion below.
        assert!(
            p.a_real_write_fails(),
            "ground truth: with the read end closed a real write must fail (EPIPE). If it did \
             not, this fixture never created the state `is_writable` is being asked about"
        );
        assert!(
            !is_writable(p.write),
            "the write end of a pipe with no reader must be reported UNwritable — this is the \
             `kastellan-cli guard capture … | head` case, where `eprintln!` panics and \
             `panic = \"abort\"` turns it into a silent SIGABRT (#733)"
        );
    }

    /// A descriptor number that was open, is now closed, and that no
    /// concurrently running test can reclaim.
    ///
    /// ⚠️ **The NUMBER is the whole fixture, and the obvious version of this
    /// test is flaky.** Its first draft closed a `pipe(2)` and probed fds 3
    /// and 4. `open(2)` hands out the **lowest free** descriptor and libtest
    /// runs this module's other fixtures on parallel threads —
    /// [`a_live_pipe_is_writable`] opens a pipe,
    /// [`a_regular_file_is_writable`] a tempfile — so fd 3 was routinely
    /// reopened between the close and the probe, and `poll` then correctly
    /// reported a live descriptor. Measured on macOS: **10 failures in 10**
    /// runs of `cargo test -p kastellan-core --lib worker_stderr`, the narrow
    /// form CLAUDE.md documents, and **0 in 10** full-workspace sweeps, where
    /// fd 3 is long since taken. A sweep-only green is exactly how this would
    /// have shipped. The lowest-free rule cannot hand back a number this high
    /// while the binary holds a few dozen descriptors.
    const UNRECLAIMABLE_FD: RawFd = 900;

    /// Open [`UNRECLAIMABLE_FD`], prove it is open, then close it.
    fn a_closed_descriptor() -> RawFd {
        let p = Pipe::new();
        assert!(
            unsafe { libc::dup2(p.write, UNRECLAIMABLE_FD) } >= 0,
            "the test needs to place a descriptor at fd {UNRECLAIMABLE_FD}; this is a host \
             problem, not a code one"
        );
        // Ground truth in the other direction: it really is open right now, so
        // the assertion below is about the CLOSE and not about a number that
        // was never valid.
        assert!(
            is_open(UNRECLAIMABLE_FD),
            "fd {UNRECLAIMABLE_FD} must be open before we close it, or this fixture never \
             created the transition it is testing"
        );
        unsafe { libc::close(UNRECLAIMABLE_FD) };
        assert!(
            !is_open(UNRECLAIMABLE_FD),
            "fd {UNRECLAIMABLE_FD} must be closed after `close`; if it is not, another thread \
             reclaimed it and this fixture is racing"
        );
        UNRECLAIMABLE_FD
    }

    #[test]
    fn a_descriptor_that_was_never_open_is_not_writable() {
        // `2>&-` leaves fd 2 closed. `poll` reports POLLNVAL rather than an
        // error, so this is a distinct arm from the broken-pipe one — and
        // since #745's review, POLLNVAL is believed only when `fcntl` agrees,
        // so this also pins the corroboration step.
        let fd = a_closed_descriptor();
        assert!(
            !is_writable(fd),
            "a closed descriptor ({fd}) must be reported UNwritable; `2>&-` is the shape"
        );
    }

    #[test]
    fn a_character_device_is_writable() {
        // ⚠️ REGRESSION GUARD, macOS-specific and measured. Darwin's `poll`
        // sets POLLNVAL on a perfectly usable character device, so the
        // original bare `POLLNVAL` arm reported `/dev/null` UNwritable while a
        // real write to it succeeded — fail-CLOSED, which suppressed every
        // worker report in any process run `2>/dev/null`. That is the one
        // direction this module's doc forbids.
        //
        // Linux does not share the quirk, so on Linux this test passes with or
        // without the corroboration and cannot prove it. It is still the right
        // place for the assertion: the mask is one piece of code and it has to
        // be correct on both hosts. Same argument as the POLLERR/POLLHUP
        // split, which `is_writable`'s doc tabulates.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("/dev/null must be openable");
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        // Ground truth FIRST: the kernel really will accept a write here, so a
        // `false` below is the probe being wrong rather than the fixture.
        assert!(
            (&file).write(b"x").is_ok(),
            "ground truth: a real write to /dev/null must succeed, or this fixture is not \
             testing what it claims"
        );
        assert!(
            is_writable(fd),
            "/dev/null must be reported writable. On macOS `poll` answers POLLNVAL for a live \
             character device, so a bare POLLNVAL arm suppresses every report from a process \
             run `2>/dev/null` — the silence this module exists to remove. POLLNVAL must stay \
             corroborated by `fcntl(F_GETFD)`"
        );
    }

    #[test]
    fn a_regular_file_is_writable() {
        // The deployed daemon's stderr is a file or a journal socket, not a
        // tty. If `is_writable` were to answer `false` for a file, #734's fix
        // would land the daemon back in silence — the opposite of its point.
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = std::fs::File::create(dir.path().join("stderr")).expect("create the file");
        assert!(
            is_writable(std::os::fd::AsRawFd::as_raw_fd(&file)),
            "a regular file must be reported writable — this is how the daemon's stderr is \
             usually wired"
        );
    }

    #[test]
    fn this_test_binarys_own_stderr_is_writable() {
        // The case every other test in the tree depends on. A probe that said
        // `false` here would silently strip the marked fallback line out of
        // both e2e suites, which assert on its presence — but this test names
        // the reason, so the failure points at the probe rather than at them.
        assert!(
            is_writable(libc::STDERR_FILENO),
            "the test binary's own stderr must be reported writable"
        );
    }
}
