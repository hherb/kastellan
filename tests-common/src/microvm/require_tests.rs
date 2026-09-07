//! Unit tests for [`super::require`] — the REQUIRE-aware precondition
//! vocabulary (#679) and the source-level guard that keeps the call sites
//! using it.
//!
//! Two halves, and the second is the load-bearing one. The pure combinators
//! are easy to get right; what #667 actually shipped broken was the *wiring*,
//! and #680's review found the wiring could be replaced with `false` with the
//! whole suite still green. So the guard below reads the real
//! `core/tests/*.rs` sources and fails on any precondition that bypasses the
//! knob — including ones nobody has written yet.

use super::require::{
    dep_or_skip, dep_or_skip_to, first_unmet, host_probe_order, host_probes, skip_unless_ready,
    skip_unless_ready_to, Probe,
};
use crate::env::{env_lock, EnvVarGuard};
use crate::microvm::REQUIRE_ENV;

/// A probe that is met (no reason).
fn met() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// first_unmet — pure
// ---------------------------------------------------------------------------

/// All met → nothing to report. The case an operator on a good host hits
/// every run, so it must not cost a `[SKIP]`.
#[test]
fn first_unmet_is_none_when_every_probe_is_met() {
    let probes: [Probe; 3] = [&met, &met, &met];
    assert_eq!(first_unmet(&probes), None);
}

/// The empty slice is "nothing to check", not "something is wrong". A caller
/// that has no extra preconditions must be able to pass `&[]` without
/// inventing a dummy probe.
#[test]
fn first_unmet_is_none_for_no_probes() {
    assert_eq!(first_unmet(&[]), None);
}

/// The FIRST unmet reason wins, in the order given. Ordering is the caller's
/// statement of which diagnosis is more useful: "no supervisor" explains a
/// missing Postgres cluster, not the other way round.
#[test]
fn first_unmet_returns_the_first_unmet_reason_in_order() {
    let a = || Some("supervisor unavailable: no bus".to_string());
    let b = || Some("no Postgres install found".to_string());
    let probes: [Probe; 3] = [&met, &a, &b];
    assert_eq!(
        first_unmet(&probes).as_deref(),
        Some("supervisor unavailable: no bus"),
        "the first unmet probe is the reported one"
    );
}

/// Short-circuiting is a behaviour, not an optimisation: these probes SPAWN
/// (`default_probe` shells out; `origin_unreachable_reason` opens a TCP
/// connection with a 5 s timeout PER RESOLVED ADDRESS). A probe after the
/// first failure must not run at all, or an offline host pays seconds to be
/// told something it already knew.
#[test]
fn first_unmet_does_not_evaluate_probes_past_the_first_failure() {
    use std::cell::Cell;
    let ran_later = Cell::new(false);
    let fails = || Some("bwrap probe failed: no userns".to_string());
    let later = || {
        ran_later.set(true);
        None
    };
    let probes: [Probe; 2] = [&fails, &later];
    let _ = first_unmet(&probes);
    assert!(!ran_later.get(), "probes after the first failure must not be evaluated");
}

// ---------------------------------------------------------------------------
// skip_unless_ready — the Skip arm and the Fail arm
// ---------------------------------------------------------------------------

/// Unset knob: the unmet precondition renders as a real `[SKIP]` line and the
/// caller returns. This is the pre-#679 behaviour, unchanged — a plain
/// `cargo test` on a host with no supervisor must stay green.
#[test]
fn skip_unless_ready_emits_a_skip_line_and_returns_true_when_the_knob_is_unset() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let skipped = skip_unless_ready_to(&probes, &mut sink);

    assert!(skipped, "an unmet precondition means the caller returns");
    let rendered = String::from_utf8(sink).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must emit the auditable line: {rendered:?}");
    assert!(rendered.contains("no bus"), "must carry the reason: {rendered:?}");
}

/// All met: no line at all. A `[SKIP]` line is evidence in this tree
/// (`grep -c '^\[SKIP\]'` audits a run), so a helper that printed on the
/// success path would inflate exactly the count it protects.
#[test]
fn skip_unless_ready_is_silent_and_returns_false_when_every_probe_is_met() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let probes: [Probe; 2] = [&met, &met];
    assert!(!skip_unless_ready_to(&probes, &mut sink));
    assert!(sink.is_empty(), "a met precondition prints nothing: {sink:?}");
}

/// The whole point of #679: with the knob truthy, the precondition the
/// operator did NOT ask about still stops the run. Before this, a host with
/// KVM, vsock, a built launcher and fresh images but no `enable-linger` gave
/// the operator a green run they had explicitly tried to rule out.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn skip_unless_ready_panics_naming_the_knob_when_a_real_run_was_demanded() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let mut sink: Vec<u8> = Vec::new();
    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let _ = skip_unless_ready_to(&probes, &mut sink);
}

/// ...and the panic must carry the *reason*, not just the knob, or the
/// operator is told their run was refused without being told what to fix.
#[test]
#[should_panic(expected = "no bus")]
fn skip_unless_ready_panic_carries_the_reason() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let mut sink: Vec<u8> = Vec::new();
    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let _ = skip_unless_ready_to(&probes, &mut sink);
}

// ---------------------------------------------------------------------------
// dep_or_skip — the value-returning sibling
// ---------------------------------------------------------------------------

/// The happy path hands the value straight through and prints nothing.
#[test]
fn dep_or_skip_passes_the_value_through_silently() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let got: Option<u32> = dep_or_skip_to(Ok(7), &mut sink);
    assert_eq!(got, Some(7));
    assert!(sink.is_empty(), "a met dependency prints nothing: {sink:?}");
}

/// Unset knob: `None` plus the auditable line, so the `let ... else return`
/// call sites keep working exactly as they did.
#[test]
fn dep_or_skip_reports_and_returns_none_when_the_knob_is_unset() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::unset(REQUIRE_ENV);

    let mut sink: Vec<u8> = Vec::new();
    let got: Option<u32> = dep_or_skip_to(Err("no Postgres install found".to_string()), &mut sink);
    assert_eq!(got, None);
    let rendered = String::from_utf8(sink).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must emit the auditable line: {rendered:?}");
    assert!(rendered.contains("Postgres"), "must carry the reason: {rendered:?}");
}

/// A missing *dependency* is as fatal under REQUIRE as a failed probe. Both
/// are "this run would prove nothing"; that they arrive as `Result` rather
/// than `Option<String>` is a shape difference, not a severity difference.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn dep_or_skip_panics_when_a_real_run_was_demanded() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let mut sink: Vec<u8> = Vec::new();
    let _: Option<u32> = dep_or_skip_to(Err("no Postgres install found".to_string()), &mut sink);
}

// ---------------------------------------------------------------------------
// The stderr-writing wrappers
// ---------------------------------------------------------------------------
//
// Everything above tests the `_to(out)` seam. The wrappers are what the 10 real
// call sites actually call, and #680's review is precisely the lesson that an
// untested one-line delegation is a place a knob quietly stops working: both
// `skip_unless_ready(_) -> false` and `dep_or_skip(d) -> d.ok()` survived the
// whole suite before these two tests existed, the second silently deleting the
// `[SKIP]` line AND the panic from every micro-VM suite in the tree.
//
// The panic is observable without capturing stderr, so proving delegation costs
// one `#[should_panic]` each.

/// [`super::require::skip_unless_ready`] really delegates to the tested seam.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn the_skip_unless_ready_wrapper_reaches_the_knob() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let fails = || Some("supervisor unavailable: no bus".to_string());
    let probes: [Probe; 1] = [&fails];
    let _ = skip_unless_ready(&probes);
}

/// [`super::require::dep_or_skip`] really delegates to the tested seam.
#[test]
#[should_panic(expected = "KASTELLAN_MICROVM_REQUIRE_E2E")]
fn the_dep_or_skip_wrapper_reaches_the_knob() {
    let _lock = env_lock();
    let _guard = EnvVarGuard::set(REQUIRE_ENV, "1");

    let _: Option<u32> = dep_or_skip(Err("no Postgres install found".to_string()));
}

// ---------------------------------------------------------------------------
// host_probes — the pair, and its order
// ---------------------------------------------------------------------------

/// The pair is supervisor-then-sandbox, and the order is documented as a claim
/// about which diagnosis is more useful.
///
/// Asserted by *name*, because neither of the two obvious alternatives works:
/// calling the probes is vacuous on a healthy host (both return `None`, so a
/// swap passes), and comparing `&fn_item` addresses is meaningless because a
/// fn item is zero-sized and every reference to one shares an address. That
/// second attempt is why `host_probes` carries a name-tagged table at all.
#[test]
fn host_probes_reports_the_supervisor_before_the_sandbox() {
    assert_eq!(
        host_probe_order(),
        ["supervisor", "sandbox"],
        "the supervisor is probed first: enable-linger is a prerequisite for the PG cluster \
         every one of these sites brings up next, so it is the more useful diagnosis"
    );
    assert_eq!(host_probes().len(), 2, "the pair every daemon e2e asks for");
}
