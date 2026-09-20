//! What the system *says* about a worker that died: the report formatters, the
//! stderr-fallback markers, and the emitters that write to both channels.
//!
//! Split out of [`super`] (the capture half) because the two answer different
//! questions. That module keeps a dying worker's bytes; this one turns them
//! into a line a human reads. Every item here is re-exported by the parent, so
//! `worker_stderr::` is still the one public path and no call site names
//! `report` directly.
//!
//! # Two channels, one producer per event
//!
//! A worker's last words have to reach whoever is looking, and *who* that is
//! differs by binary. The daemon installs a `tracing` subscriber as the first
//! statement of `main`; **test binaries and `kastellan-cli` install none**, so
//! before #725 a report logged through `tracing` alone went nowhere in exactly
//! the place a human was reading it. Each emitter here therefore logs through
//! `tracing` *and* `eprintln!`s a marked line when no subscriber exists.
//!
//! There are two such events, and they get **distinct markers** so a grep can
//! tell them apart:
//!
//! | Event | Marker | Emitter |
//! | --- | --- | --- |
//! | a tool worker exits before answering | [`EARLY_EXIT_STDERR_MARKER`] | [`emit_early_exit_report`] |
//! | a persistent worker dies mid-service | [`WORKER_DEATH_STDERR_MARKER`] | [`emit_persistent_death_report`] |
//!
//! ⚠️ **The shared half is shared on purpose.** Both emitters go through the
//! same private [`format_stderr_fallback`] and [`emit_to_stderr_when_unheard`],
//! so neither the neutralisation nor the `has_been_set()` guard exists in two
//! copies that can drift. That drift is the shape CLAUDE.md's bwrap-argv note
//! names, where the incomplete copy is the one that breaks — and #730 is the
//! worked example: the persistent-worker report was a hand-rolled second copy
//! of the `tracing`-only half and inherited none of #725's fixes.

use std::process::ExitStatus;

/// Human-readable one-line summary of a worker's death for the daemon log: the
/// exit status (which distinguishes a clean `exit status: 1` — a deliberate
/// fail-loud exit — from a `signal: 6 (SIGABRT)` — a crash) plus the recent
/// stderr lines, joined for a single log record.
pub fn format_death_report(status: Option<ExitStatus>, stderr_tail: &[String]) -> String {
    let status_str = match status {
        Some(s) => s.to_string(),
        None => "exit status unknown (not yet reaped)".to_string(),
    };
    if stderr_tail.is_empty() {
        format!("worker exited ({status_str}); no stderr captured")
    } else {
        format!("worker exited ({status_str}); recent stderr: {}", stderr_tail.join(" | "))
    }
}

/// Explain an `EarlyExit` — the protocol client's "worker exited before
/// responding" — using the worker's own retained stderr.
///
/// `EarlyExit` is the single most content-free failure this system produces: it
/// says the pipe closed and nothing else. Three separate micro-VM production
/// defects all surfaced as exactly this string, and telling them apart took a
/// session (#666). The worker almost always *did* say why, on the stream nobody
/// was reading — this turns that stream into the message.
///
/// The empty case is deliberately not silent, and does not read like the
/// populated one: "said nothing" is itself a diagnosis (a hard kill, a jail
/// refused before the worker ran, a guest that never booted), and it points at
/// a different place to look than a worker that logged a reason.
pub fn format_early_exit_report(program: &str, method: &str, stderr_tail: &[String]) -> String {
    if stderr_tail.is_empty() {
        format!(
            "worker {program} exited before responding to {method}, and wrote NOTHING to \
             stderr — suspect a kill (wall-clock/OOM/seccomp) or a sandbox that refused the \
             spawn before the worker ran, rather than the worker's own logic"
        )
    } else {
        format!(
            "worker {program} exited before responding to {method}; its last words: {}",
            stderr_tail.join(" | ")
        )
    }
}

/// The [`format_early_exit_report`] counterpart for a worker whose stderr was
/// never piped, so there is no tail to quote.
///
/// A separate function rather than a third arm of the one above, because the
/// *input* is different: that one is given a tail and reports on its contents,
/// including when it is empty (a worker that ran and said nothing — a kill, a
/// refused spawn). This one is reached when no tail exists at all, which is a
/// statement about our own spawn path and not about the worker.
///
/// Still names the program and the method, which is the half that survives:
/// on the micro-VM path the process that exits is `kastellan-microvm-run` and
/// the tool is whatever ran inside the guest, so "which worker" is not
/// recoverable from the caller's tool name.
///
/// ⚠️ **Defensive, and currently unreachable.** Its caller's `None` arm fires
/// only when `SupervisedWorker` has no stderr tail, and all four sandbox
/// backends spawn with `stderr(Stdio::piped())` while `spawn_worker` is the
/// only constructor. So the test below pins this wording, not a reachable
/// path — worth stating plainly rather than letting a future reader mistake
/// coverage here for coverage of the arm.
pub fn format_unpiped_early_exit_report(program: &str, method: &str) -> String {
    format!(
        "worker {program} exited before responding to {method}; its stderr was not piped, \
         so there is nothing to report"
    )
}

/// Marker prefixing every line [`emit_early_exit_report`] writes to the
/// process's own stderr, so the fallback is greppable and distinguishable from
/// the `tracing` rendering of the same text.
///
/// Deliberately **not** one of this tree's three evidence markers (`[SKIP]`,
/// `[WARN]`, `[E2E]`). `scripts/run-e2e-gate.sh` asserts **zero** `[WARN]`
/// lines in a profile run, and every profile passes `--nocapture`, so a line
/// this function emits during a gate run reaches the gate log even from a
/// passing test. Any suite that provoked an early exit deliberately would then
/// turn every profile red. Nothing in a profile does so today — the separation
/// is cheap insurance, not a description of current suites.
///
/// ⚠️ The gate's greps are **anchored at line start**, so the test beside this
/// asserts the marker does not *begin with* one of the three — equality would
/// let `[WARN] …` through.
pub const EARLY_EXIT_STDERR_MARKER: &str = "[worker-early-exit]";

/// The exact bytes the stderr fallback writes. Pure, so the rendering can be
/// pinned without a process that has no subscriber.
///
/// ⚠️ **Neutralises here, not only in the caller.** The one-line property the
/// marker doc above argues for — that this can never produce a second, column-0
/// line a gate grep reads as `[SKIP]`/`[WARN]`/`[E2E]` — has to belong to the
/// function that *renders the line*, not to the call order inside
/// [`emit_early_exit_report`]. This is `pub`, and the obvious second producer
/// already exists: the persistent-worker death report
/// ([#730](https://github.com/hherb/kastellan/issues/730)) has the same defect
/// one layer over. A caller that reached for this formatter and neutralised
/// nothing would hand a model-authored `\n` straight to a gate log — the
/// drift-between-copies shape CLAUDE.md's bwrap-argv note names, where the
/// incomplete copy is the one that breaks.
///
/// `neutralise_controls` is idempotent and length-preserving, so
/// `emit_early_exit_report` neutralising first costs nothing and both orders
/// give identical bytes.
pub fn format_early_exit_stderr_fallback(report: &str) -> String {
    format!(
        "{EARLY_EXIT_STDERR_MARKER} {}",
        crate::untrusted_text::neutralise_controls(report)
    )
}

/// Report a worker's early exit through `tracing`, **and** through the
/// process's own stderr when no `tracing` subscriber is installed.
///
/// The one producer, so every caller gets both channels (#725).
///
/// **Who gets the fallback.** The daemon installs a subscriber as the first
/// statement of `main` (`core/src/main.rs`), before any dispatch, so it never
/// fires there and the daemon's behaviour is unchanged. It is *not* limited to
/// tests, though: `kastellan-cli` installs no subscriber anywhere, and
/// `kastellan-cli guard capture` dispatches the real web-fetch worker
/// (`core/src/bin/kastellan-cli/guard_capture.rs`). That command gains the
/// line, which is the intent — it is operator-facing and has no log for the
/// report to reach.
///
/// **Test binaries install none**, and that is the case this exists for: 29 of
/// the 30 `core/tests/*.rs` suites that dispatch to a real worker never call
/// `tracing_subscriber` (count: suites calling `dispatch`/`dispatch_with_sink`,
/// the only path here), so #666's report went nowhere in precisely the place a
/// human was reading. #719 is the worked example — a session spent on it with
/// the cause still unknown, then one `tracing_subscriber` line in the failing
/// test and the next run printed the Python traceback that named it.
///
/// ⚠️ **`eprintln!`, not `writeln!(std::io::stderr(), …)`.** libtest captures a
/// test's output through `std::io::set_output_capture`, which the
/// `print!`/`eprint!` **macros** consult and the `Stdout`/`Stderr` handles do
/// not. (The default panic hook consults it too, which is why a panic message
/// also lands in the captured block.) Writing to the handle directly goes to
/// the real file descriptor, bypasses the capture, and so never appears under
/// the failing test that needs it. Pinned end-to-end — by reading the child's
/// two streams *separately* — in
/// `core/tests/worker_early_exit_stderr_fallback_e2e.rs`.
///
/// Log-only in both channels, and deliberately so: the tail is raw worker
/// output, and the dispatch chokepoint scrubs redeemed secrets out of
/// everything that reaches the planner and the audit row (audit H1). These
/// bytes go to the process's own stderr and nowhere else — never into a
/// returned error, the planner, or an audit row.
///
/// ⚠️ **The whole report is neutralised here, not just the tail.** Tail lines
/// are already control-stripped as they enter the ring (`push_trimmed`), but
/// `program` and `method` are interpolated raw by the formatters above, and
/// `method` can be **model-authored**: `qualified_method` returns `None` for a
/// method outside the tool's advertised set and the caller then passes the
/// planner's string verbatim. Without this, a `\n` in it could forge a
/// column-0 line in a gate log and an ESC could drive the terminal reading it.
/// `neutralise_controls` is idempotent, so the already-clean tail is
/// unaffected, and it keeps the report to exactly one line.
///
/// This call covers the **`tracing`** channel.
/// [`format_early_exit_stderr_fallback`] neutralises again for its own line —
/// deliberately not relying on this one, because it is `pub` and a second
/// producer must not be able to bypass the property by calling it directly.
/// Both orders give identical bytes.
///
/// ⚠️ **One subscriber anywhere in a binary silences the fallback for all of
/// it.** `tracing::dispatcher::has_been_set` is a process-global `AtomicBool`
/// that `with_default` sets as well as `set_global_default`, and which is
/// **never cleared** — dropping a scoped guard does not re-enable the
/// fallback. So it cannot distinguish "this thread has a subscriber" from
/// "some other test installed one earlier". That is the conservative
/// direction — a binary with a subscriber loses nothing, it just reads the
/// report through `tracing` — but a suite that installs one in one test and
/// expects the fallback in another will not get it.
///
/// ⚠️ **It also asks the wrong question.** "Is a subscriber installed" is not
/// "will this WARN be recorded": an installed subscriber that *filters the
/// event out* drops it on both channels, silently. That is reachable in the
/// deployed daemon through a target-scoped `RUST_LOG` in the operator overlay.
/// [#734](https://github.com/hherb/kastellan/issues/734) tracks moving to a
/// delivery check (`event_enabled!`).
///
/// ⚠️ `has_been_set` is `#[doc(hidden)]` in `tracing` and carries no stability
/// guarantee. It is the only way to ask the question, and its removal would be
/// a compile error rather than a silent behaviour change; a change in its
/// meaning would be caught by the e2e above.
///
/// ⚠️ `eprintln!` panics if the write fails (a closed or broken stderr), and
/// the guard is `!has_been_set()` — *not* "am I the daemon". So the exposed set
/// is every binary without a subscriber: the test binaries, and
/// `kastellan-cli`. `kastellan-cli guard capture … 2>&1 | head` gives the
/// reader an `EPIPE` (Rust sets `SIGPIPE` to `SIG_IGN`), and under
/// `panic = "abort"` that is a silent `SIGABRT` rather than a message. It is
/// not a regression — `guard_capture` already prints with these macros
/// throughout, so the same pipeline already aborts on its very first
/// diagnostic — but this line is on an ERROR path, where losing the run costs
/// more. Tracked in
/// [#733](https://github.com/hherb/kastellan/issues/733).
pub fn emit_early_exit_report(report: &str) {
    let report = crate::untrusted_text::neutralise_controls(report);
    tracing::warn!("{report}");
    if !tracing::dispatcher::has_been_set() {
        eprintln!("{}", format_early_exit_stderr_fallback(&report));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn death_report_no_status_no_stderr() {
        let report = format_death_report(None, &[]);
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
        let report = format_death_report(Some(status), &["boom".into(), "trace".into()]);
        assert!(report.contains("exit status"), "{report}");
        assert!(report.contains("boom | trace"), "{report}");
    }

    #[test]
    fn early_exit_report_carries_the_workers_last_words() {
        let r = format_early_exit_report(
            "kastellan-microvm-run",
            "python.exec",
            &["microvm-init: relay UDS bind failed".to_string()],
        );
        assert!(r.contains("kastellan-microvm-run"), "{r}");
        assert!(r.contains("python.exec"), "{r}");
        assert!(r.contains("relay UDS bind failed"), "{r}");
    }

    #[test]
    fn early_exit_report_says_so_when_the_worker_was_silent() {
        // Silence is a different diagnosis, not a missing one — it must point
        // somewhere (a kill, a refused spawn) rather than read as an absence.
        let r = format_early_exit_report("w", "m", &[]);
        assert!(r.contains("NOTHING"), "{r}");
        assert!(r.contains("kill"), "silence must name where to look: {r}");
    }

    #[test]
    fn unpiped_early_exit_report_still_names_which_worker_and_which_method() {
        // The one thing this arm can still say. It is reached when the backend
        // handed us no stderr pipe, so there is no tail to quote — but on the
        // micro-VM path the process that exits is the launcher and the tool is
        // whatever ran in the guest, so "which worker" is not recoverable from
        // the caller's tool name and dropping it would leave nothing at all.
        let r = format_unpiped_early_exit_report("kastellan-microvm-run", "python.exec");
        assert!(r.contains("kastellan-microvm-run"), "{r}");
        assert!(r.contains("python.exec"), "{r}");
        assert!(r.contains("not piped"), "it must say WHY there is no tail: {r}");
    }

    #[test]
    fn the_stderr_fallback_line_is_marked_and_keeps_the_report_whole() {
        // The marker makes the fallback greppable and tells a reader which
        // channel they are looking at; the report must survive it intact,
        // because the report is the entire point of the line.
        let report = format_early_exit_report("w", "m", &["last words".to_string()]);
        let line = format_early_exit_stderr_fallback(&report);
        assert!(
            line.starts_with(EARLY_EXIT_STDERR_MARKER),
            "the marker must lead, so a grep anchored at line start finds it: {line}"
        );
        assert!(line.contains(&report), "the report must not be altered: {line}");
    }

    #[test]
    fn the_stderr_fallback_marker_is_distinctive_and_not_a_gate_evidence_marker() {
        // `scripts/run-e2e-gate.sh` asserts ZERO `[WARN]` lines in a profile
        // run, and every profile passes `--nocapture`, so a line this module
        // emits reaches the gate log even from a PASSING test. Borrowing one of
        // the three evidence markers would therefore turn a profile red for a
        // suite that was working.
        //
        // ⚠️ No profile selects an early-exit suite today — see the const's own
        // doc, which is the accurate statement. This is insurance against the
        // profile that adds one, not a description of current suites; an
        // earlier version of this comment claimed the opposite and contradicted
        // the const two hundred lines up.
        //
        // `starts_with`, not equality: the gate's greps are anchored at line
        // start (`grep -c '^\[WARN\]'`), so a marker of `"[WARN] early-exit"`
        // would pass an inequality check and still trip the assertion.
        for marker in ["[SKIP]", "[WARN]", "[E2E]"] {
            assert!(
                !EARLY_EXIT_STDERR_MARKER.starts_with(marker),
                "the fallback marker must not BEGIN with the gate evidence marker {marker}: \
                 the gate greps them anchored at line start"
            );
        }
        // Without this the check above is vacuous: `""` — and any marker that
        // is a strict prefix of all three, like `"["` — satisfies every
        // `starts_with` in this file, and `stdout.contains("")` in the e2e is
        // true of any output at all. Every other assertion on the marker reads
        // the const, so this is the only place its VALUE is pinned.
        assert!(
            EARLY_EXIT_STDERR_MARKER.starts_with("[worker-")
                && EARLY_EXIT_STDERR_MARKER.ends_with(']')
                && EARLY_EXIT_STDERR_MARKER.len() > "[worker-]".len(),
            "the marker must be a non-trivial bracketed `[worker-…]` token; an empty or \
             single-character marker passes every other check in this file vacuously, \
             including `contains` in the e2e. Got: {EARLY_EXIT_STDERR_MARKER:?}"
        );
    }

    #[test]
    fn the_stderr_fallback_neutralises_a_model_authored_control_character() {
        // `program` and `method` are interpolated RAW by the formatters above
        // (tail lines are already stripped by `push_trimmed`), and `method` is
        // model-authored: `qualified_method` returns `None` outside the tool's
        // advertised set and the planner's string then goes through verbatim.
        //
        // This pins the property on the PUB formatter rather than on
        // `emit_early_exit_report`'s call order, so a second producer — #730's
        // persistent-worker death report is the one already filed — cannot
        // render an un-neutralised line by reaching for this function.
        let report = format_early_exit_report(
            "w",
            "m\u{1b}[31m\n[WARN] forged-gate-line",
            &["last words".to_string()],
        );
        let line = format_early_exit_stderr_fallback(&report);
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
}
