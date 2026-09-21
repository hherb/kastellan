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
/// not in the class), so [`emit_early_exit_report`] neutralising first costs
/// nothing and both orders give identical bytes.
///
/// ⚠️ It is **char-count** preserving, not byte-length preserving — U+2028 is
/// 3 bytes in and 1 out. This said "length-preserving", which in Rust reads as
/// `.len()` and is false; the conclusion above needs only idempotency.
pub fn format_early_exit_stderr_fallback(report: &str) -> String {
    format_stderr_fallback(EARLY_EXIT_STDERR_MARKER, report)
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
    emit_to_stderr_when_unheard(EARLY_EXIT_STDERR_MARKER, &report);
}

/// Marker prefixing every line [`emit_persistent_death_report`] writes to the
/// process's own stderr.
///
/// Deliberately **distinct from** [`EARLY_EXIT_STDERR_MARKER`], because the two
/// events point somewhere different. An early exit says a tool worker never
/// answered a *specific call* — look at that call's jail, its arguments, its
/// wall clock. A persistent-worker death says a *long-lived* worker that had
/// been serving fine stopped, and the supervisor is now respawning it — look at
/// the respawn-rate alarm and at what changed underneath it. Reusing one marker
/// for both would make a gate log unable to answer which of those happened.
///
/// Shares the `[worker-…]` shape so a reader who wants *either* can grep
/// `^\[worker-`; see [`STDERR_FALLBACK_MARKERS`] for the whole set.
pub const WORKER_DEATH_STDERR_MARKER: &str = "[worker-death]";

/// Every marker this module can put at the start of a fallback line.
///
/// Exists so the rules that bind *all* fallback markers — not a gate evidence
/// marker, non-trivial, distinct from each other — are asserted over a set
/// rather than over one name that a second marker could quietly fail to join.
///
/// ⚠️ **A new marker must be added here as well as declared.** Nothing forces
/// it: a third `pub const` that never joins this array is a line in a gate log
/// that no test ever looked at. The tests below are the only enforcement, and
/// they can only check what the array holds.
pub const STDERR_FALLBACK_MARKERS: [&str; 2] =
    [EARLY_EXIT_STDERR_MARKER, WORKER_DEATH_STDERR_MARKER];

/// Pure: the exact bytes a marked stderr-fallback line carries.
///
/// **The one renderer for both markers.** Parameterising the marker rather than
/// writing a second `format!` is the point: the neutralisation below then exists
/// in exactly one place, and a future third marker inherits it by construction
/// instead of by whoever adds it remembering.
///
/// ⚠️ **Neutralises here, not only in the callers.** The one-line property —
/// that a fallback can never produce a second, column-0 line a gate grep reads
/// as `[SKIP]`/`[WARN]`/`[E2E]` — has to belong to the function that *renders
/// the line*, not to any caller's call order. `neutralise_controls` is
/// idempotent, so a caller that also neutralises (both do, for their `tracing`
/// half) costs nothing and both orders give identical bytes. (Idempotent and
/// char-count preserving — NOT byte-length preserving; U+2028 is 3 bytes in,
/// 1 out.)
fn format_stderr_fallback(marker: &str, report: &str) -> String {
    format!("{marker} {}", crate::untrusted_text::neutralise_controls(report))
}

/// Write `report` to the process's own stderr, marked, **when no `tracing`
/// subscriber is installed** — the shared second channel behind both emitters.
///
/// See [`emit_early_exit_report`] for the full argument about who gets this and
/// why it must be `eprintln!`; that doc is the canonical one and is not repeated
/// here. The short version: libtest captures through `std::io::set_output_capture`,
/// which the `print!`/`eprint!` **macros** consult and the `Stdout`/`Stderr`
/// handles do not, so `writeln!(std::io::stderr(), …)` would never appear under
/// the failing test that needs it.
fn emit_to_stderr_when_unheard(marker: &str, report: &str) {
    if !tracing::dispatcher::has_been_set() {
        eprintln!("{}", format_stderr_fallback(marker, report));
    }
}

/// Pure: the persistent-worker death line, with `label` folded into the text.
///
/// ⚠️ **The label is in the message, not only in a `tracing` field.** The daemon's
/// subscriber is `tracing_subscriber::fmt().with_env_filter(…).json()`
/// (`core/src/main.rs`) — the filter half named explicitly because it is what
/// #734 below is about, and omitting it let two notes in this file disagree. So
/// `%label` is a queryable JSON field worth keeping — but the stderr fallback
/// carries no fields at all. A label that lived only in the field would leave the
/// fallback saying "persistent worker died" without naming *which*, and the two
/// production users are `matrix` and `email`. So it goes in both places: the
/// field for the daemon's log queries, the text so the line stands alone.
pub fn format_persistent_death_line(label: &str, report: &str) -> String {
    format!("persistent worker {label} died: {report}")
}

/// The [`format_early_exit_stderr_fallback`] counterpart for a persistent
/// worker's death report.
///
/// ⚠️ **Takes an ALREADY-FOLDED line, not a raw `death_report()` string — the
/// two counterparts are not symmetric.** The early-exit formatter is handed a
/// self-contained report that already names its worker. This one is not: the
/// label is folded in by [`format_persistent_death_line`], and
/// [`emit_persistent_death_report`] passes *that* result here. A caller who
/// passes a bare `death_report()` string instead gets
/// `[worker-death] worker exited (…)` with **no label** — precisely the failure
/// `format_persistent_death_line`'s own doc argues must never happen, since the
/// fallback channel carries no `tracing` fields to recover it from.
pub fn format_persistent_death_stderr_fallback(report: &str) -> String {
    format_stderr_fallback(WORKER_DEATH_STDERR_MARKER, report)
}

/// Report a persistent worker's death through `tracing`, **and** through the
/// process's own stderr when no subscriber is installed.
///
/// The one producer for that event (#730), mirroring [`emit_early_exit_report`]
/// for the early-exit one. Before this, `worker_lifecycle::persistent`'s driver
/// called `tracing::warn!` directly — a hand-rolled copy of the `tracing`-only
/// half that inherited none of #725's work. The persistent path is how the
/// **Matrix** and **email** channel workers run, so in a test binary those died
/// in silence: the tail was captured, the report was rendered, and then it was
/// discarded because nothing was listening.
///
/// ⚠️ **Neutralisation here is defence-in-depth at the trait boundary, not a
/// live hole — say so rather than imply otherwise.** Unlike the early-exit
/// report, whose `method` is genuinely model-authored, every input reaching this
/// today is already safe: `ClientTransport::death_report` renders an
/// `ExitStatus` plus a tail that `push_trimmed` neutralised on the way into the
/// ring, and every `label` in the tree is a literal. What makes it worth doing
/// anyway is that [`PersistentTransport::death_report`] is a **public trait
/// method** — any implementor is a producer of this string, and
/// `egress::persistent_net` already delegates through it — so the guarantee
/// belongs at the point the line is rendered rather than in an audit of today's
/// implementors.
///
/// [`PersistentTransport::death_report`]: crate::worker_lifecycle::PersistentTransport::death_report
pub fn emit_persistent_death_report(label: &str, report: &str) {
    let label = crate::untrusted_text::neutralise_controls(label);
    let line = crate::untrusted_text::neutralise_controls(&format_persistent_death_line(
        &label, report,
    ));
    tracing::warn!(%label, "{line}");
    emit_to_stderr_when_unheard(WORKER_DEATH_STDERR_MARKER, &line);
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
    fn every_stderr_fallback_marker_is_distinctive_and_not_a_gate_evidence_marker() {
        // `scripts/run-e2e-gate.sh` asserts ZERO `[WARN]` lines in a profile run,
        // and every profile passes `--nocapture`, so a line this module emits
        // reaches the gate log even from a PASSING test. Borrowing one of the three
        // evidence markers would therefore turn a profile red for a suite that was
        // working.
        //
        // ⚠️ No profile selects an early-exit or worker-death suite today — see the
        // consts' own docs, which are the accurate statement. This is insurance
        // against the profile that adds one, not a description of current suites.
        for marker in STDERR_FALLBACK_MARKERS {
            // `starts_with`, not equality: the gate's greps are anchored at line
            // start (`grep -c '^\[WARN\]'`), so a marker of `"[WARN] early-exit"`
            // would pass an inequality check and still trip the assertion.
            for evidence in ["[SKIP]", "[WARN]", "[E2E]"] {
                assert!(
                    !marker.starts_with(evidence),
                    "the fallback marker {marker:?} must not BEGIN with the gate evidence marker \
                     {evidence}: the gate greps them anchored at line start"
                );
            }
            // Without this the check above is vacuous: `""` — and any marker that
            // is a strict prefix of all three, like `"["` — satisfies every
            // `starts_with` in this file, and `stdout.contains("")` in the e2es is
            // true of any output at all. Every other assertion on a marker reads
            // the const, so this is the only place their VALUES are pinned.
            assert!(
                marker.starts_with("[worker-")
                    && marker.ends_with(']')
                    && marker.len() > "[worker-]".len(),
                "each marker must be a non-trivial bracketed `[worker-…]` token; an empty or \
                 single-character marker passes every other check in this file vacuously, \
                 including `contains` in the e2es. Got: {marker:?}"
            );
        }
    }

    #[test]
    fn the_two_stderr_fallback_markers_are_distinct() {
        // An early exit and a persistent-worker death point at different places to
        // look — a single call's jail versus a long-lived worker that stopped and is
        // being respawned. One marker for both would make a gate log unable to say
        // which happened, which is the entire reason #730 got its own rather than
        // reusing `EARLY_EXIT_STDERR_MARKER`.
        //
        // Reads the ARRAY, not the two consts, so a third marker that duplicated an
        // existing one is caught here too.
        let mut seen = STDERR_FALLBACK_MARKERS.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(
            seen.len(), before,
            "every fallback marker must be distinct, or a reader cannot tell the events apart: \
             {STDERR_FALLBACK_MARKERS:?}"
        );

        // ⚠️ **Distinct is not enough — no marker may PREFIX another.** Both
        // e2e suites collect their lines with `line.starts_with(MARKER)`, so a
        // pair like `[worker-death]` / `[worker-death-persistent]` would be
        // unequal (passing the dedup above) while silently folding one suite's
        // lines into the other's `marked` vector, and a `marked.len() == 1`
        // assertion would then be counting someone else's output. Equality is
        // the wrong relation to test when every consumer uses prefixes.
        for (i, a) in STDERR_FALLBACK_MARKERS.iter().enumerate() {
            for (j, b) in STDERR_FALLBACK_MARKERS.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    !b.starts_with(a),
                    "fallback marker {b:?} begins with {a:?}; every consumer matches these with \
                     `starts_with`, so one suite's lines would be collected as the other's"
                );
            }
        }
    }

    #[test]
    fn the_persistent_death_line_names_which_worker_died_and_carries_the_report() {
        // The fallback channel carries no `tracing` fields, so a label that lived
        // only in `%label` would leave the stderr line saying "persistent worker
        // died" without naming which — and the two production users are `matrix`
        // and `email`, i.e. exactly the distinction an operator needs first.
        let report = "worker exited (exit status: 1); recent stderr: boom";
        let line = format_persistent_death_line("matrix", report);
        assert!(line.contains("matrix"), "the line must name WHICH worker died: {line}");
        assert!(
            line.contains("boom"),
            "the report must survive intact — it is the point: {line}"
        );
        assert!(line.contains("died"), "the line must say what happened: {line}");
        // ⚠️ The SHAPE, not just the ingredients. Three `contains` checks pass
        // just as well on `"persistent worker {report} died: {label}"` — the
        // two interpolations swapped — which compiles (both are `&str`) and
        // renders an operator a line naming the exit status as the worker and
        // the worker as the cause. That mutant survived the whole suite until
        // the #735 review; this is what kills it.
        assert_eq!(
            line,
            format!("persistent worker matrix died: {report}"),
            "the label and the report must land in their own slots: {line}"
        );
    }

    #[test]
    fn the_persistent_death_fallback_neutralises_a_hostile_report() {
        // ⚠️ Defence-in-depth at a PUBLIC TRAIT BOUNDARY, not a live hole — the
        // emitter's doc says so plainly and this test does not claim otherwise.
        // Nothing reaching this today is attacker-controlled (an `ExitStatus`, plus
        // a tail already stripped by `push_trimmed`), but
        // `PersistentTransport::death_report` is `pub` and `egress::persistent_net`
        // already delegates through it, so any implementor is a producer of this
        // string. Pinning the property on the PUB formatter — rather than on
        // `emit_persistent_death_report`'s call order — is what stops such an
        // implementor rendering an un-neutralised line by reaching for it directly.
        let line = format_persistent_death_stderr_fallback(
            "worker exited\u{1b}[31m\n[WARN] forged-gate-line",
        );
        assert!(
            line.starts_with(WORKER_DEATH_STDERR_MARKER),
            "the marker must lead, so a grep anchored at line start finds it: {line}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "an ESC would be an ANSI sequence executing in the reader's terminal: {line:?}"
        );
        assert_eq!(
            line.lines().count(),
            1,
            "the fallback must be exactly ONE line — a `\\n` here forges a column-0 line that \
             `run-e2e-gate.sh`'s `^\\[WARN\\]` grep counts, failing an unrelated profile: {line:?}"
        );
        // Neutralisation maps the class to a space, so the text must SURVIVE —
        // mid-line. A check that merely asserted its absence would also pass if the
        // report stopped reaching the line at all.
        assert!(
            line.contains("forged-gate-line"),
            "the text must survive as text; only its line-forging effect is removed: {line:?}"
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
        // `emit_early_exit_report`'s call order, so a second producer cannot
        // render an un-neutralised line by reaching for this function. That
        // second producer now exists — the persistent-worker death report
        // (#730) — and the measurement is worth recording: dropping the
        // neutralisation from the SHARED renderer leaves both e2e suites green,
        // because each emitter neutralises its own line first and masks it.
        // Only this pair of unit tests dies. A `pub` renderer's guarantee is
        // reachable from a unit test and from nowhere else.
        //
        // ⚠️ **Say what that mutant is, plainly: near-equivalent today.** Both
        // `format_*_stderr_fallback` wrappers have ZERO callers outside these
        // tests — `emit_to_stderr_when_unheard` calls the private renderer
        // directly — so the neutralisation these two tests defend is redundant
        // belt-and-braces in every shipped binary. That makes this a test of a
        // `pub` API's contract for a future caller, which is worth having, and
        // NOT evidence that the e2e suites have a hole. Reading it the other
        // way would overstate what the pair proves.
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
