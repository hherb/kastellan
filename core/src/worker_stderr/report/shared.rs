//! The half both events share: the marker census, the one line renderer, and
//! the no-subscriber guard.
//!
//! Kept in its own module so the neutralisation and the `has_been_set()` check
//! exist in exactly one place. Two copies that agree today are the drift shape
//! CLAUDE.md's bwrap-argv note names, and #730 is this tree's worked example.

use super::persistent::WORKER_DEATH_STDERR_MARKER;
use super::tool_worker::EARLY_EXIT_STDERR_MARKER;

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
pub(super) fn format_stderr_fallback(marker: &str, report: &str) -> String {
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
pub(super) fn emit_to_stderr_when_unheard(marker: &str, report: &str) {
    if !tracing::dispatcher::has_been_set() {
        eprintln!("{}", format_stderr_fallback(marker, report));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

}
