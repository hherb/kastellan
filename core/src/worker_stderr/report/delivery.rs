//! Whether a report actually **reaches** someone, and whether writing it can
//! kill the process.
//!
//! Two questions that the three emitters in this module all have to answer the
//! same way, and which used to be answered by one line —
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
/// * `POLLHUP` — measured on macOS for the write end of a pipe whose read end
///   closed (a real `write_all` on that same descriptor returns `BrokenPipe`).
/// * `POLLERR` — what Linux sets for that same case; `poll(2)` names it
///   explicitly for "the write end of a pipe when the read end has been
///   closed". Testing only the bit the development host happens to set is the
///   shape that ships a guard which is inert on the deployment host.
/// * `POLLNVAL` — the descriptor is not open at all, which is what `2>&-`
///   leaves behind.
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
    pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0
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

/// Emit `report` on **both** channels from the **caller's** module.
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
        if !::tracing::event_enabled!(::tracing::Level::WARN) {
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
        if !::tracing::event_enabled!(::tracing::Level::WARN, label) {
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
    /// designs apart, which is what [`the_check_answers_for_the_EMITTERS_target`]
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
    #[allow(non_snake_case)]
    fn the_check_answers_for_the_EMITTERS_target() {
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

    #[test]
    fn a_descriptor_that_was_never_open_is_not_writable() {
        // `2>&-` leaves fd 2 closed. `poll` reports POLLNVAL rather than an
        // error, so this is a distinct arm from the broken-pipe one.
        let (read, write) = {
            let p = Pipe::new();
            let fds = (p.read, p.write);
            drop(p); // closes both
            fds
        };
        for fd in [read, write] {
            assert!(
                !is_writable(fd),
                "a closed descriptor ({fd}) must be reported UNwritable; `2>&-` is the shape"
            );
        }
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
