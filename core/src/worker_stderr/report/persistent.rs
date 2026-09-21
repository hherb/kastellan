//! What the system says about a **persistent** (long-lived) worker: the death
//! report body, the `[worker-death]` marker, and the emitter behind it.
//!
//! Split out of [`super`] by *event*, not by layer: everything here answers
//! "a long-lived worker that had been serving stopped". Its sibling
//! [`super::tool_worker`] answers "a tool worker did not answer this call",
//! and [`super::shared`] holds the one renderer both go through.

use std::process::ExitStatus;

use super::shared::{emit_to_stderr_when_unheard, format_stderr_fallback};
#[allow(unused_imports)] // referenced by the marker doc's intra-doc link
use super::shared::STDERR_FALLBACK_MARKERS;

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

}
