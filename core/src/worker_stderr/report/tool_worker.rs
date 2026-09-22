//! What the system says about a **tool** worker that did not answer a call:
//! the report bodies, the `[worker-failed]` marker, and the emitter.
//!
//! Covers all five fatal `ClientError` variants, not just `EarlyExit` (#737).
//! The classification itself is not here — it is
//! [`WorkerRetirementCause`], beside the retirement decision that reads the
//! same census.
//!
//! Split out of [`super`] by *event*, not by layer — see
//! [`super::persistent`] for the long-lived-worker counterpart and
//! [`super::shared`] for the one renderer both go through.

use crate::worker_lifecycle::idle_timeout::WorkerRetirementCause;

use super::delivery::warn_and_fall_back;
use crate::worker_stderr::CapturedTail;
use super::shared::format_stderr_fallback;
#[allow(unused_imports)] // referenced by the marker doc's intra-doc link
use super::shared::STDERR_FALLBACK_MARKERS;
#[allow(unused_imports)] // referenced by this module's doc links
use super::persistent::format_persistent_death_stderr_fallback;

/// Pure: the clause naming what the worker did wrong, with the method folded in.
///
/// One arm per [`WorkerRetirementCause`], because the five point a reader at
/// five different places. Lumping them under "exited before responding" — which
/// is what the tree did before #737 for the one variant it reported at all —
/// would be actively misleading for `ResponseTooLarge`, where the worker did
/// not exit and may still be running.
fn happened_clause(cause: WorkerRetirementCause, program: &str, method: &str) -> String {
    match cause {
        WorkerRetirementCause::ExitedBeforeResponding => {
            format!("worker {program} exited before responding to {method}")
        }
        WorkerRetirementCause::PipeBroke => {
            format!("worker {program}'s stdio pipe failed while it was answering {method}")
        }
        WorkerRetirementCause::Undecodable => {
            format!("worker {program} answered {method} with bytes that are not a JSON-RPC response")
        }
        WorkerRetirementCause::IdMismatch => {
            format!("worker {program} answered {method} with the wrong request id")
        }
        WorkerRetirementCause::ResponseTooLarge => {
            format!("worker {program} answered {method} with a response over the record cap")
        }
    }
}

/// Pure: where to look when the worker left no explanation behind.
///
/// Silence is itself a diagnosis, and a different one per cause — that is the
/// property the `EarlyExit` wording already had and which #737 has to keep
/// while generalising. A worker that exited silently was probably killed; a
/// worker that is *still running* and sent garbage was not, so pointing the
/// reader at OOM and seccomp there would send them to the wrong place.
fn silent_hint(cause: WorkerRetirementCause) -> &'static str {
    match cause {
        // Byte-for-byte the wording `format_early_exit_report` carried before
        // #737 generalised it; the e2e and unit tests pin it.
        WorkerRetirementCause::ExitedBeforeResponding | WorkerRetirementCause::PipeBroke => {
            "suspect a kill (wall-clock/OOM/seccomp) or a sandbox that refused the spawn \
             before the worker ran, rather than the worker's own logic"
        }
        WorkerRetirementCause::Undecodable => {
            "suspect the worker printing to stdout instead of stderr — anything on stdout \
             that is not one JSON-RPC record per line corrupts the stream"
        }
        WorkerRetirementCause::IdMismatch => {
            "suspect the worker answering out of order or replying twice; the request and \
             response streams are now out of step"
        }
        WorkerRetirementCause::ResponseTooLarge => {
            "suspect the tool returning unbounded output; the unread remainder leaves the \
             read stream desynced, so the worker is retired rather than reused"
        }
    }
}

/// Explain why a tool worker is being retired, using its own retained stderr.
///
/// **The one report body for all five fatal `ClientError` variants** (#737).
/// Before it, only `EarlyExit` was ever reported and the other four were retired
/// in silence on every channel — see [`WorkerRetirementCause::from_client_error`],
/// which is the census both this and the retirement decision read.
///
/// `EarlyExit` is the single most content-free failure this system produces: it
/// says the pipe closed and nothing else. Three separate micro-VM production
/// defects all surfaced as exactly this string, and telling them apart took a
/// session (#666). The worker almost always *did* say why, on the stream nobody
/// was reading — this turns that stream into the message.
///
/// # The three tails are three different statements
///
/// `stderr_tail` is a tri-state, and collapsing any two of them loses a
/// diagnosis:
///
/// | Value | Means | Reads |
/// | --- | --- | --- |
/// | `Some` complete, lines | the worker explained itself | "its last words: …" |
/// | `Some` complete, empty | the worker ran and said nothing | "wrote NOTHING …" + a per-cause hint |
/// | `Some` PARTIAL, lines | it was still talking when we stopped listening | "its stderr so far … may be its FIRST lines" |
/// | `Some` PARTIAL, empty | we did not wait long enough to know | "PARTIAL capture … absence is NOT evidence" |
/// | `None` | our own spawn path piped no stderr | "its stderr was not piped …" |
///
/// The second is a diagnosis (a hard kill, a jail refused before the worker
/// ran, a guest that never booted) and points somewhere different from the
/// first, which is a statement about *us* and not about the worker.
///
/// ⚠️ **The two PARTIAL rows are #732, and they are the reason this takes a
/// [`CapturedTail`] rather than a slice.** With only the lines to go on, an
/// empty tail whose drain timed out is indistinguishable from a worker that
/// stayed silent — and this function would then print the "suspect a kill"
/// diagnosis for a worker that explained itself 10 ms after we stopped
/// listening. Never collapse the flag back into the slice.
///
/// ⚠️ **The `None` arm is defensive and currently unreachable.** It fires only
/// when `SupervisedWorker` has no stderr tail, and all four sandbox backends
/// spawn with `stderr(Stdio::piped())` while `spawn_worker` is the only
/// constructor. The test below pins this wording, not a reachable path — worth
/// stating plainly rather than letting a future reader mistake coverage here for
/// coverage of the arm.
///
/// Whichever arm runs, the report still names the program *and* the method: on
/// the micro-VM path the process that exits is `kastellan-microvm-run` and the
/// tool is whatever ran inside the guest, so "which worker" is not recoverable
/// from the caller's tool name.
pub fn format_worker_failure_report(
    program: &str,
    method: &str,
    cause: WorkerRetirementCause,
    stderr_tail: Option<&CapturedTail>,
) -> String {
    let what = happened_clause(cause, program, method);
    let ms = crate::worker_stderr::TAIL_DRAIN_WAIT.as_millis();
    match stderr_tail {
        None => format!("{what}; its stderr was not piped, so there is nothing to report"),
        // KNOWN silent — the drain reached EOF and there was nothing in it. Only
        // here may the report name a cause.
        Some(t) if t.is_known_silent() => {
            format!("{what}, and wrote NOTHING to stderr — {}", silent_hint(cause))
        }
        // Nothing arrived, but we stopped waiting. Saying "wrote NOTHING" here
        // sends the reader after a kill/seccomp/OOM fault that may not exist.
        Some(t) if t.lines().is_empty() => format!(
            "{what}, and its stderr drain did not reach EOF within {ms} ms, so NOTHING was \
             captured yet — this is a PARTIAL capture, and the absence of a reason here is \
             NOT evidence that there was none"
        ),
        // Some lines, but more may have been coming. The ring evicts OLDEST
        // first, so an incomplete tail holds the worker's FIRST words, not its
        // last — calling them "last words" points the reader at startup noise.
        Some(t) if !t.is_complete() => format!(
            "{what}; its stderr so far (the drain did not reach EOF within {ms} ms, so these \
             may be its FIRST lines rather than its last): {}",
            t.lines().join(" | ")
        ),
        Some(t) => format!("{what}; its last words: {}", t.lines().join(" | ")),
    }
}

/// Marker prefixing every line [`emit_worker_failure_report`] writes to the
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
pub const WORKER_FAILED_STDERR_MARKER: &str = "[worker-failed]";

/// The exact bytes the early-exit stderr fallback writes. Pure, so the
/// rendering can be pinned without a process that has no subscriber.
///
/// ⚠️ **Neutralises — in `format_stderr_fallback`, which it delegates to, not
/// in its callers.** (Named, not linked: that renderer is private, and linking
/// to it from a `pub` item's docs makes rustdoc warn.) The one-line property
/// the marker doc above argues for —
/// that this can never produce a second, column-0 line a gate grep reads as
/// `[SKIP]`/`[WARN]`/`[E2E]` — belongs to the function that *renders the line*,
/// never to a caller's call order. This is `pub`, so a caller that reached for
/// it directly and neutralised nothing would otherwise hand a model-authored
/// `\n` straight to a gate log.
///
/// The second producer this warned about while it was hypothetical now exists
/// and is [`format_persistent_death_stderr_fallback`]
/// ([#730](https://github.com/hherb/kastellan/issues/730)). It is deliberately
/// a sibling delegating to the *same* renderer rather than a second `format!`:
/// that is the drift-between-copies shape CLAUDE.md's bwrap-argv note names,
/// where the incomplete copy is the one that breaks.
///
/// `neutralise_controls` is idempotent (it maps the class to `' '`, which is
/// not in the class), so [`emit_worker_failure_report`] neutralising first costs
/// nothing and both orders give identical bytes.
///
/// ⚠️ It is **char-count** preserving, not byte-length preserving — U+2028 is
/// 3 bytes in and 1 out. This said "length-preserving", which in Rust reads as
/// `.len()` and is false; the conclusion above needs only idempotency.
pub fn format_worker_failure_stderr_fallback(report: &str) -> String {
    format_stderr_fallback(WORKER_FAILED_STDERR_MARKER, report)
}

/// Report a worker's early exit on whichever channel will actually carry it:
/// `tracing` when it will record the event, the marked stderr line when it
/// will not.
///
/// The one producer, so every caller gets a channel (#725), and exactly one
/// (#734) — the two are mutually exclusive, so no reader sees the report
/// twice.
///
/// **Who gets the fallback.** The daemon installs a subscriber as the first
/// statement of `main` (`core/src/main.rs`), before any dispatch — but ⚠️ **it
/// is no longer exempt, and that is the whole of #734.** `main` builds that
/// subscriber from `EnvFilter::try_from_default_env()`, and `EnvFilter`
/// applies its default directive only when the env string is **empty**. So an
/// operator who sets a target-scoped `RUST_LOG` through the
/// `kastellan.env.local` overlay — `kastellan_core::scheduler=debug`, while
/// debugging the scheduler — makes the daemon fall through to this line like
/// any test binary. Before #734 that silenced the report on *both* channels.
/// It is *not* limited to tests either: `kastellan-cli` installs no subscriber
/// anywhere, and
/// `kastellan-cli guard capture` dispatches the real web-fetch worker
/// (`core/src/bin/kastellan-cli/guard_capture.rs`). That command gains the
/// line, which is the intent — it is operator-facing and has no log for the
/// report to reach.
///
/// **Test binaries install none**, and that is the case this exists for: 29 of
/// the 31 `core/tests/*.rs` suites that dispatch to a real worker never call
/// `tracing_subscriber` (count: suites calling `dispatch`/`dispatch_with_sink`,
/// the only path here), so #666's report went nowhere in precisely the place a
/// human was reading.
///
/// ⚠️ **Re-count the denominator when adding a suite; this one went stale
/// once already.** It read "29 of the 30" until the #735 review measured it:
/// #731 added `worker_early_exit_stderr_fallback_e2e.rs`, which both dispatches
/// *and* installs a subscriber, so it joined **both** sides and the ratio moved
/// 29/30 → 29/31 while the numerator stayed coincidentally right. The
/// persistent-worker emitter below deliberately cites a **different** census
/// (`PersistentWorker::spawn*` suites), because the two populations are
/// disjoint — see `worker_lifecycle::persistent`'s emit site.
///
/// #719 is the worked example — a session spent on it with
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
/// [`format_worker_failure_stderr_fallback`] neutralises again for its own line —
/// deliberately not relying on this one, because it is `pub` and a second
/// producer must not be able to bypass the property by calling it directly.
/// Both orders give identical bytes.
///
/// ⚠️ **The delivery decision and the abort guard both live in
/// `report/delivery.rs`, and this doc deliberately does not restate them.**
/// (Named as a file, not linked: `mod delivery` is private, and a link from
/// this `pub fn` makes rustdoc warn — the same rule `worker_stderr`'s module
/// doc states for `report/`.)
/// That module owns `warn_and_fall_back!` (why the check must expand at *this*
/// callsite, and why every field the `warn!` carries — `message` included —
/// must be named in it) and `is_writable` (why a broken stderr is probed
/// before `eprintln!`, and the measured `EBADF`-vs-`EPIPE` split). Two copies
/// of that reasoning is the drift shape CLAUDE.md's bwrap-argv note names, and
/// this block was previously a worked example of it: it described the
/// `has_been_set()` guard for a whole release after that guard was deleted,
/// and cited #734 and #733 as open after both had shipped.
///
/// Returns **whether the stderr fallback line was written** — that is, whether
/// `tracing` was not going to carry this report. Every production caller
/// discards it with a `;`.
///
/// ⚠️ **The return value exists so the REAL emitter is testable, and that is
/// not a nicety.** The macro's placement tests use a stand-in emitter in their
/// own module, which proves the mechanism but says nothing about *this*
/// function — a refactor that replaced the macro here with a plain call would
/// pass every one of them. A guard that only ever examines its own fixture
/// shares that fixture's blind spot; returning the verdict lets a test point at
/// the function that actually ships.
pub fn emit_worker_failure_report(report: &str) -> bool {
    let report = crate::untrusted_text::neutralise_controls(report);
    warn_and_fall_back!(WORKER_FAILED_STDERR_MARKER, &report)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_five_tail_states_all_read_differently() {
        // Five states, five sentences. Any two that render alike have silently
        // collapsed a distinction the caller went to trouble to preserve — the
        // same failure `no_two_causes_read_the_same` guards for causes.
        let c = WorkerRetirementCause::ExitedBeforeResponding;
        let lines = vec!["boom".to_string()];
        let rendered: Vec<String> = [
            Some(CapturedTail::complete(lines.clone())),
            Some(CapturedTail::complete(vec![])),
            Some(CapturedTail::partial(lines)),
            Some(CapturedTail::partial(vec![])),
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
    fn the_three_tail_states_are_three_different_statements() {
        // Collapsing any two loses a diagnosis: "said nothing" is a kill or a
        // refused spawn, while "not piped" is a statement about OUR spawn path
        // and says nothing about the worker at all.
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
}
