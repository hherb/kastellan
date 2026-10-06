//! Whether a report actually **reaches** someone, and whether writing it can
//! kill the process.
//!
//! Two questions that the five emitters in `report`'s sibling modules
//! (`tool_worker`, `persistent`, `refusal`) all have to answer the same way — none of them
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
/// `POLLHUP` split above: one host cannot prove it. `fcntl(F_GETFD)` answers
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
/// discard it with a `;`. It exists because the tests in `delivery/tests.rs` have to observe
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
    // One `label` field at an ERROR level: the refusal report's credential
    // line (#783), which names an operator action. Same shape as the WARN arm below.
    // The level is spelled out rather than passed in, because `tracing` bakes
    // the level into a static callsite, and because the check must name the
    // SAME level as the event: an `error!` checked at WARN would answer for a
    // different directive set.
    ($marker:expr, $line:expr, label = $label:expr, level = ERROR $(,)?) => {{
        let line: &str = $line;
        let label: &str = $label;
        ::tracing::error!(%label, "{line}");
        if !::tracing::event_enabled!(::tracing::Level::ERROR, label, message) {
            $crate::worker_stderr::report::delivery::write_fallback_line(
                &$crate::worker_stderr::report::shared::format_stderr_fallback($marker, line),
            )
        } else {
            false
        }
    }};
    // The ERROR arm above, with the MARKER in the traced message as well
    // (#828): the `[audit-lost]` report. An operator's alert keys on that
    // marker, and with the plain arm it was in the stderr fallback only, so on
    // a daemon whose `tracing` records ERROR — the default — a grep for it
    // matched nothing however many rows were lost. Here both paths carry the
    // same bytes (`format_stderr_fallback` once), so one alert rule sees every
    // report whatever the operator's `RUST_LOG`. The worker markers keep the
    // plain arms: their tracing lines are keyed by `label`, and no alert reads
    // them by marker.
    ($marker:expr, $line:expr, label = $label:expr, level = ERROR, marked $(,)?) => {{
        let marked = $crate::worker_stderr::report::shared::format_stderr_fallback($marker, $line);
        let label: &str = $label;
        ::tracing::error!(%label, "{marked}");
        if !::tracing::event_enabled!(::tracing::Level::ERROR, label, message) {
            $crate::worker_stderr::report::delivery::write_fallback_line(&marked)
        } else {
            false
        }
    }};
    // One `label` field at INFO: the refusal report's recovery line (#788),
    // which says a refusal ended. Same shape, and the same reason the level
    // is spelled out, as the ERROR arm above: an `info!` checked at WARN
    // would answer "recorded" under a WARN-only filter that drops it, and
    // the line would reach nobody.
    ($marker:expr, $line:expr, label = $label:expr, level = INFO $(,)?) => {{
        let line: &str = $line;
        let label: &str = $label;
        ::tracing::info!(%label, "{line}");
        if !::tracing::event_enabled!(::tracing::Level::INFO, label, message) {
            $crate::worker_stderr::report::delivery::write_fallback_line(
                &$crate::worker_stderr::report::shared::format_stderr_fallback($marker, line),
            )
        } else {
            false
        }
    }};
    // One `label` field: both persistent-worker reports and the refusal
    // report's WARN line. The field is named in the check as well as in the
    // `warn!`, which is the whole point of having this arm rather than falling
    // through to the first.
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
mod tests;
