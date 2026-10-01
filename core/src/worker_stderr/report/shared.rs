//! The half all five events share: the marker census and the one line
//! renderer.
//!
//! Kept in its own module so the neutralisation exists in exactly one place.
//! Two copies that agree today are the drift shape CLAUDE.md's bwrap-argv note
//! names, and #730 is this tree's worked example.
//!
//! ⚠️ **The emit half deliberately does NOT live here** — it is
//! [`super::delivery::warn_and_fall_back`], a macro, because the delivery check
//! has to expand in the emitter's own module to answer for the emitter's
//! target. A version of it written here answered `true` for events `EnvFilter`
//! had dropped; the measured table is on that macro.

use super::audit_lost::AUDIT_LOST_STDERR_MARKER;
use super::persistent::{WORKER_DEATH_STDERR_MARKER, WORKER_DOWN_STDERR_MARKER};
use super::refusal::WORKER_REFUSAL_STDERR_MARKER;
use super::tool_worker::WORKER_FAILED_STDERR_MARKER;

/// Every marker this module can put at the start of a fallback line.
///
/// Exists so the rules that bind *all* fallback markers — not a gate evidence
/// marker, non-trivial, distinct from each other — are asserted over a set
/// rather than over one name that a second marker could quietly fail to join.
///
/// ⚠️ **A new marker must be added here as well as declared.** Nothing forces
/// it: a **sixth** `pub const` that never joins this array is a line in a gate
/// log that no test ever looked at. The tests below are the only enforcement, and
/// they can only check what the array holds.
pub const STDERR_FALLBACK_MARKERS: [&str; 5] = [
    WORKER_FAILED_STDERR_MARKER,
    WORKER_DEATH_STDERR_MARKER,
    WORKER_DOWN_STDERR_MARKER,
    WORKER_REFUSAL_STDERR_MARKER,
    AUDIT_LOST_STDERR_MARKER,
];

/// Pure: the exact bytes a marked stderr-fallback line carries.
///
/// **The one renderer for every marker.** Parameterising the marker rather than
/// writing a second `format!` is the point: the neutralisation below then exists
/// in exactly one place, and a future marker inherits it by construction
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
pub(crate) fn format_stderr_fallback(marker: &str, report: &str) -> String {
    format!("{marker} {}", crate::untrusted_text::neutralise_controls(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The families a fallback marker may belong to. Adding one is deliberate.
    const MARKER_FAMILIES: [&str; 2] = ["worker", "audit"];

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
            //
            // `[<family>-<event>]`, both halves non-empty, the family one of
            // `MARKER_FAMILIES`: `[worker-…]` for the four worker events,
            // `[audit-lost]` for #792's, which is not one. A closed list, not
            // "any lower-case word" (#800): a new family is a decision to record
            // here, not a typo like `[abc-x]` that passes unnoticed.
            let token = marker.strip_prefix('[').and_then(|m| m.strip_suffix(']'));
            let halves = token.and_then(|t| t.split_once('-'));
            assert!(
                halves.is_some_and(|(family, event)| {
                    MARKER_FAMILIES.contains(&family) && !event.is_empty()
                }),
                "each marker must be a non-trivial bracketed `[<family>-<event>]` token (a \
                 family from MARKER_FAMILIES, a non-empty event); an empty or single-character marker \
                 passes every other check in this file vacuously, including `contains` in the \
                 e2es. Got: {marker:?}"
            );
        }
    }

    #[test]
    fn the_stderr_fallback_markers_are_distinct() {
        // The five events point at five different places to look: a single
        // call's jail (`[worker-failed]`), a long-lived worker that stopped and
        // is being respawned (`[worker-death]`), one the supervisor is NOT
        // getting back (`[worker-down]`), and a live one whose upstream said
        // no (`[worker-refusal]`, #783). One marker for any two of them would
        // make a gate log unable to say which happened — the reason #730 gave
        // the death its own rather than reusing the tool-worker marker, and
        // #738 the same again.
        //
        // Reads the ARRAY, not the consts, so a new marker that duplicated an
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
