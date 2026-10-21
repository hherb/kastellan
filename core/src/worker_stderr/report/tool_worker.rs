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
use crate::worker_stderr::{CapturedTail, TailState};
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
/// # The seven tails are seven different statements
///
/// `stderr_tail` is a **seven**-state input, and collapsing any two of them
/// loses a diagnosis:
///
/// | Value | Means | Reads |
/// | --- | --- | --- |
/// | `TailState::LastWords` | the worker explained itself | "its last words: …" |
/// | `TailState::KnownSilent` | the worker ran and said nothing | "wrote NOTHING …" + a per-cause hint |
/// | `TailState::FirstWords` | it was still talking when we stopped listening | "its stderr so far … may be its FIRST lines" |
/// | `TailState::NothingCapturedYet` | we did not wait long enough to know | "PARTIAL capture … absence is NOT evidence" |
/// | `TailState::DrainFailedWords` | our read of its pipe failed part-way | "up to the point our drain FAILED …" |
/// | `TailState::DrainFailedSilent` | our read of its pipe failed at byte 0 | "the silence is in our pipe …" |
/// | `None` | our own spawn path piped no stderr | "its stderr was not piped …" |
///
/// (`TailState` is `pub(crate)`, so its variants are named, not linked.)
///
/// `KnownSilent` is the only **diagnosis** here (a hard kill, a jail refused
/// before the worker ran, a guest that never booted). It points somewhere
/// quite different from the three empty states below it —
/// `TailState::NothingCapturedYet`, `TailState::DrainFailedSilent` and the
/// `None` arm — each of which renders an identical *absence* of lines but is a
/// statement about **us**, not about the worker.
///
/// ⚠️ **The four non-EOF rows are #732 and #747, and they are the reason this
/// takes a [`CapturedTail`] rather than a slice.** With only the lines to go
/// on, an empty tail whose drain timed out — or whose drain *failed* — is
/// indistinguishable from a worker that stayed silent, and this function would
/// then print the "suspect a kill" diagnosis for a worker that explained
/// itself 10 ms after we stopped listening. Never collapse the state back into
/// the slice.
///
/// ⚠️ **Matching `CapturedTail::state` is what keeps that true.** This used
/// to be a guard chain over `is_known_silent()` / `is_complete()` whose
/// correctness rested on arm *order*, documented and unchecked; #732 and #746
/// are both renderers that got that order wrong. A missing arm here is now a
/// compile error.
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
    let Some(tail) = stderr_tail else {
        return format!("{what}; its stderr was not piped, so there is nothing to report");
    };
    match tail.state() {
        // KNOWN silent — the drain reached EOF and there was nothing in it.
        // Only here may the report name a cause.
        TailState::KnownSilent => {
            format!("{what}, and wrote NOTHING to stderr — {}", silent_hint(cause))
        }
        // Nothing arrived, but we stopped waiting. Saying "wrote NOTHING" here
        // sends the reader after a kill/seccomp/OOM fault that may not exist.
        TailState::NothingCapturedYet => format!(
            "{what}, and its stderr drain did not reach EOF within {ms} ms, so NOTHING was \
             captured yet — this is a PARTIAL capture, and the absence of a reason here is \
             NOT evidence that there was none"
        ),
        // The read itself failed, so the emptiness is ours (#747). Pointing at
        // the pipe rather than at the worker is the whole difference: waiting
        // longer would not have helped, and auditing seccomp would find
        // nothing.
        // The guess comes from the errno, and only when it names one (#752).
        TailState::DrainFailedSilent(fault) => format!(
            "{what}, and our read of its stderr FAILED ({fault}) before anything arrived, so \
             NOTHING was captured — the silence is in our pipe and is NOT evidence the worker \
             was quiet{}",
            fault.suspect().map(|s| format!("; {s}")).unwrap_or_default()
        ),
        // Some lines, but more may have been coming. The ring evicts OLDEST
        // first, so an incomplete tail holds the worker's FIRST words, not its
        // last — calling them "last words" points the reader at startup noise.
        TailState::FirstWords(lines) => format!(
            "{what}; its stderr so far (the drain did not reach EOF within {ms} ms, so these \
             may be its FIRST lines rather than its last): {}",
            lines.join(" | ")
        ),
        // Same doubt as above, different cause — and unlike a timeout, this
        // one will not resolve by waiting (#747).
        TailState::DrainFailedWords(fault, lines) => format!(
            "{what}; its stderr up to the point our read of the pipe FAILED ({fault}; so these may be \
             its FIRST lines rather than its last, and there may have been more we could \
             never read): {}",
            lines.join(" | ")
        ),
        TailState::LastWords(lines) => format!("{what}; its last words: {}", lines.join(" | ")),
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
mod tests;
