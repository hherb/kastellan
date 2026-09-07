//! The guard that reads the **real** `core/tests/*.rs` sources and fails on
//! any micro-VM precondition that bypasses `KASTELLAN_MICROVM_REQUIRE_E2E`
//! (#679).
//!
//! # Why this is a source-level test and not a unit test
//!
//! #667 added the knob; #680's review then found the *wiring* of that knob
//! could be replaced with `false` and the suite stayed green, because every
//! test aimed at the pure half. The defect #679 records is one step further
//! out again: the knob works perfectly and the call site simply asks
//! something else first. No unit test of [`super::guard`] can see that,
//! and no Firecracker run can either — the false green only appears on a host
//! where the micro-VM preconditions are MET and a neighbouring one is not,
//! which is by definition not the host anybody is gating on.
//!
//! Reading the sources catches it, catches it for call sites nobody has
//! written yet, and runs on **both** hosts — which matters here, because the
//! micro-VM test bodies it scans are all Linux-gated (14 of the 15 files at
//! file scope; `web_research_search_broker_e2e.rs` per item) and so are
//! invisible to a Mac `cargo test`.
//!
//! ⚠️ **A scan is only as good as what it scans, and only as good as its own
//! reporting.** Three separate vacuity traps are closed here, each because
//! this tree has hit that shape before:
//!
//! * file discovery is asserted against a known-minimum roster *before* the
//!   scan is believed — a glob matching nothing would make every assertion
//!   below it pass;
//! * the scan-and-report chain has a **positive control**
//!   ([`the_scan_reports_a_planted_violation`]), because
//!   `assert!(violations.is_empty())` over a loop is green whether the loop
//!   found nothing or never ran;
//! * the roster of banned helpers is cross-checked against the helpers that
//!   actually exist, so neither list can drift.
//!
//! # What this guard does *not* cover
//!
//! The Firecracker backend only. Discovery keys on `skip_if_no_microvm`, so
//! the macOS Apple-`container` micro-VM suites are outside it — they have no
//! REQUIRE knob at all. Tracked separately; see the handover.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::repo_root;
use super::guard::{
    bypassed_gates, ends_inside_string_literal, is_skip_shaped, BANNED_HELPERS,
    REQUIRE_AWARE,
};

/// Suites that are known to gate on the micro-VM preflight, asserted present
/// before the scan is believed.
///
/// Not the full list — it is a *floor*, so adding a suite does not have to
/// touch this file, while deleting the glob's directory or breaking the
/// pattern still fails loudly.
///
/// Two entries earn their place beyond that: `matrix_firecracker_live_e2e.rs`
/// holds the tree's **only** `REQUIRE-EXEMPT` marker, and
/// `net_demo_firecracker_egress_e2e.rs` held the wrapped-`[SKIP]` bypass that
/// the first version of this guard could not see. Renaming either must not
/// quietly drop it from the scan.
const KNOWN_MICROVM_SUITES: [&str; 10] = [
    "browser_driver_firecracker_e2e.rs",
    "matrix_firecracker_live_e2e.rs",
    "net_demo_firecracker_egress_e2e.rs",
    "python_exec_firecracker_e2e.rs",
    "web_fetch_firecracker_egress_e2e.rs",
    "web_research_firecracker_broker_e2e.rs",
    "web_research_firecracker_egress_e2e.rs",
    "web_research_search_broker_e2e.rs",
    "web_research_vm_force_route_daemon_e2e.rs",
    "web_search_firecracker_egress_e2e.rs",
];

/// Every `core/tests/*.rs` that gates on [`super::REQUIRE_ENV`]'s preflight,
/// i.e. that calls `skip_if_no_microvm`.
///
/// Returns `(path, source)` pairs so a failure can name the file and the
/// caller does not read twice.
fn microvm_suites() -> Vec<(PathBuf, String)> {
    let dir = repo_root().join("core").join("tests");
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut found = Vec::new();
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        if src.contains("skip_if_no_microvm") {
            found.push((path, src));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Pure: the operator-facing violation lines for a set of `(path, source)`
/// pairs.
///
/// Split out from the test so the scan-and-report chain has a positive
/// control. `assert!(violations.is_empty())` cannot distinguish "found
/// nothing" from "never looked", which is the exact defect class this whole
/// module exists to close — leaving it unfactored would have reproduced it.
fn scan(suites: &[(PathBuf, String)]) -> Vec<String> {
    let mut violations = Vec::new();
    for (path, src) in suites {
        for gate in bypassed_gates(src) {
            violations.push(format!(
                "{}:{} — {} bypasses {}",
                path.display(),
                gate.line,
                gate.kind,
                super::REQUIRE_ENV
            ));
        }
    }
    violations
}

/// The discovery itself, asserted before it is trusted.
///
/// A `read_dir` that finds nothing, or a rename that empties the roster,
/// would leave [`every_microvm_precondition_routes_through_the_require_knob`]
/// iterating over an empty list and passing — the fail-safe-fixture shape
/// this tree has been bitten by before.
#[test]
fn the_microvm_suite_roster_is_not_empty() {
    let suites = microvm_suites();
    let names: BTreeSet<String> = suites
        .iter()
        .map(|(p, _)| p.file_name().expect("file name").to_string_lossy().into_owned())
        .collect();

    for expected in KNOWN_MICROVM_SUITES {
        assert!(
            names.contains(expected),
            "{expected} is no longer discovered as a micro-VM suite — either it was renamed \
             (update KNOWN_MICROVM_SUITES) or the scan is broken and every assertion built \
             on it is vacuous. Found: {names:?}"
        );
    }
    assert!(
        suites.len() >= KNOWN_MICROVM_SUITES.len(),
        "expected at least {} micro-VM suites, found {}",
        KNOWN_MICROVM_SUITES.len(),
        suites.len()
    );
}

/// The scanner can actually read every file it claims to scan.
///
/// `bypassed_gates` tracks string literals across lines, so a construct it does
/// not model would blank the rest of a file and make every rule below it see
/// nothing — a scan that reports clean because it went blind. Asserted per
/// file, before the verdict, and naming the file that broke.
#[test]
fn every_scanned_source_parses_to_the_end() {
    let suites = microvm_suites();
    assert!(!suites.is_empty(), "discovery is asserted non-empty elsewhere; it must hold here too");
    for (path, src) in suites {
        assert!(
            !ends_inside_string_literal(&src),
            "{} leaves the scanner mid-string-literal, so everything after that point is \
             invisible to it and a clean verdict would be meaningless. Likely a `'\"'` char \
             literal or a raw string — teach `strip_strings_and_comments` about it.",
            path.display()
        );
    }
}

/// **The #679 guard.** No micro-VM suite may ask a precondition that the
/// REQUIRE knob cannot reach.
///
/// When this fails, the fix is at the call site, not here: replace the named
/// helper with its `*_or_reason` sibling routed through
/// [`super::require::skip_unless_ready`] (a `bool` precondition) or
/// [`super::require::dep_or_skip`] (one that yields a value). A hand-written
/// `[SKIP]` that genuinely is an opt-in enablement flag rather than a host
/// precondition takes an inline `// REQUIRE-EXEMPT: <why>` marker instead.
#[test]
fn every_microvm_precondition_routes_through_the_require_knob() {
    let violations = scan(&microvm_suites());
    assert!(
        violations.is_empty(),
        "these micro-VM preconditions report a green run when {} demands a real one \
         (#679):\n  {}\n\nFix at the call site: route the reason through \
         microvm::skip_unless_ready / microvm::dep_or_skip, or mark a genuine opt-in \
         enablement flag with `// REQUIRE-EXEMPT: <why>`.",
        super::REQUIRE_ENV,
        violations.join("\n  ")
    );
}

/// The positive control for the test above.
///
/// Plants a violation into a copy of the **first real discovered source** and
/// requires it to come back named, at the right line, with the knob in the
/// message. This pins discovery, the scan and the message formatting as one
/// chain: mutate any link — scan the empty string, drop the `push`, lose the
/// line number — and this fails while the green-path assertion above would
/// not.
#[test]
fn the_scan_reports_a_planted_violation() {
    let suites = microvm_suites();
    let (path, src) = suites.first().expect("discovery is asserted non-empty elsewhere").clone();
    let planted_at = src.lines().count() + 1;
    let planted = format!("{src}if skip_if_no_supervisor() {{ return; }}\n");

    let violations = scan(&[(path.clone(), planted)]);

    assert_eq!(violations.len(), 1, "exactly the planted violation: {violations:?}");
    let only = &violations[0];
    assert!(only.contains(&path.display().to_string()), "names the file: {only}");
    assert!(only.contains(&format!(":{planted_at} ")), "names the line: {only}");
    assert!(only.contains("skip_if_no_supervisor"), "names the helper: {only}");
    assert!(only.contains(super::REQUIRE_ENV), "names the knob: {only}");
}

/// Neither list may drift from the helpers that actually exist.
///
/// [`BANNED_HELPERS`] is no longer the detection rule — [`is_skip_shaped`]
/// plus [`REQUIRE_AWARE`] is — but it is still the roster a reader consults,
/// and a roster nobody checks is a roster that rots. Both directions are
/// asserted:
///
/// * every shaped `pub fn` in `tests-common` is on **one** of the two lists,
///   so a new skip helper forces a deliberate decision (this is what caught
///   `resolve_weights_dir_or_skip`, which the first denylist missed);
/// * every [`BANNED_HELPERS`] entry still exists, so retiring a helper cannot
///   leave a name behind that a reader takes for live.
#[test]
fn the_banned_roster_matches_the_helpers_that_actually_exist() {
    let shaped = shaped_public_helpers(&repo_root().join("tests-common").join("src"));
    assert!(
        shaped.len() >= BANNED_HELPERS.len(),
        "found only {} shaped helpers in tests-common — the source walk is broken, and every \
         assertion below it is vacuous: {shaped:?}",
        shaped.len()
    );

    for name in &shaped {
        assert!(
            BANNED_HELPERS.contains(&name.as_str()) || REQUIRE_AWARE.contains(&name.as_str()),
            "`{name}` renders a [SKIP] verdict but is on neither list. Either it consults \
             KASTELLAN_MICROVM_REQUIRE_E2E (add it to REQUIRE_AWARE) or it does not, and a \
             micro-VM suite calling it would report green on a host the operator asked to \
             fail (add it to BANNED_HELPERS). Found: {shaped:?}"
        );
    }
    for banned in BANNED_HELPERS {
        assert!(
            shaped.contains(*banned),
            "BANNED_HELPERS names `{banned}`, which no longer exists in tests-common — a \
             roster entry for a dead helper reads as a live warning. Found: {shaped:?}"
        );
    }
}

/// Every `pub fn` under `dir` whose name is [`is_skip_shaped`].
///
/// Textual, like the rest of this guard, and for the same reason: the property
/// is about names, and `cfg`-gated definitions (`skip_if_no_microvm` lives
/// behind `cfg(target_os = "linux")`) must be seen from both hosts.
fn shaped_public_helpers(dir: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            found.extend(shaped_public_helpers(&path));
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        for line in src.lines() {
            let Some(rest) = line.trim_start().strip_prefix("pub fn ") else {
                continue;
            };
            let name: String =
                rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if is_skip_shaped(&name) {
                found.insert(name);
            }
        }
    }
    found
}
