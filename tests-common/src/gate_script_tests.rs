//! `scripts/run-e2e-gate.sh`, pinned against the knob vocabulary it sets.
//!
//! # Why a Rust test reads a shell script
//!
//! The gate script is the *positive control* for every gated tier, and until
//! now nothing tested it. That is the PR's own thesis turned on itself: a
//! control nobody checks is a control that can silently stop controlling.
//!
//! Two failures motivated this file specifically, and both had shipped:
//!
//! * a profile named `--test microvm_roundtrip_e2e`, **a suite that does not
//!   exist**. `cargo test --test <missing>` hard-errors, so that one was
//!   loud — but the same class quietly includes a knob variable spelled one
//!   character off, which merely does nothing.
//! * two profiles demanded `[E2E]` evidence from tiers that emitted **none**,
//!   so they failed on a perfectly healthy host. The script's own comment
//!   argues at length that a gate red on every host is a gate somebody stops
//!   running; it shipped two anyway, unnoticed because the Linux-only one is
//!   refused on the authoring Mac before it runs.
//!
//! Neither is reachable from a unit test of the Rust alone, and running the
//! real gate needs the fixtures the gate exists to demand. Reading the table is
//! what is left, and it is enough for the drift.
//!
//! ⚠️ **Every scan here carries a positive control**, because a scanner that
//! matches nothing reports a clean tree — the same unsound inference
//! (`absence of evidence` = `evidence of absence`) that [#664] is about, and
//! this file is the last place to reproduce it. If the script moves or the
//! table is reformatted, these tests fail rather than pass vacuously.
//!
//! [#664]: https://github.com/hherb/kastellan/issues/664

use std::path::PathBuf;

use crate::require::{RequireKnob, KNOBS};

/// The gate script's source.
fn gate_script() -> String {
    let path = script_path();
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a parent")
        .join("scripts/run-e2e-gate.sh")
}

/// Every `PROFILES=( … )` entry, as its raw `|`-separated spec.
///
/// Parsed rather than hand-listed for the reason the script resolves its own
/// micro-VM suites by grep: a roster copied into a test goes stale the day a
/// profile is added, and the test then reports clean over a profile it never
/// looked at.
fn profile_specs(src: &str) -> Vec<String> {
    let mut specs = Vec::new();
    let mut inside = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("PROFILES=(") {
            inside = true;
            continue;
        }
        if inside {
            if trimmed == ")" {
                break;
            }
            if let Some(body) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                specs.push(body.to_string());
            }
        }
    }
    specs
}

/// A parsed profile: only the fields these tests assert over.
struct Profile {
    name: String,
    knobs: String,
    e2e_floors: String,
    min_passed: String,
    max_skip: String,
    os: String,
    fields: usize,
}

fn parse(spec: &str) -> Profile {
    let f: Vec<&str> = spec.split('|').collect();
    let at = |i: usize| f.get(i).copied().unwrap_or_default().to_string();
    Profile {
        name: at(0),
        knobs: at(1),
        e2e_floors: at(2),
        min_passed: at(3),
        max_skip: at(4),
        os: at(7),
        fields: f.len(),
    }
}

fn profiles() -> Vec<Profile> {
    let src = gate_script();
    let specs = profile_specs(&src);
    assert!(
        specs.len() >= 4,
        "parsed {} profiles from {} — the PROFILES table moved or changed shape, and a \
         scan that finds nothing would report every check below as clean",
        specs.len(),
        script_path().display()
    );
    specs.iter().map(|s| parse(s)).collect()
}

/// The shape the script's own `validate_profiles` enforces at run time,
/// asserted here too so a malformed row fails in CI rather than only when an
/// operator next runs the gate on a staged host.
#[test]
fn every_profile_row_is_well_formed() {
    for p in profiles() {
        assert_eq!(p.fields, 8, "profile '{}' has {} fields, want 8", p.name, p.fields);
        assert!(!p.name.is_empty(), "a profile has no name");
        assert!(!p.knobs.is_empty(), "profile '{}' sets no knob", p.name);

        let min_passed: u32 = p
            .min_passed
            .parse()
            .unwrap_or_else(|_| panic!("profile '{}' MIN_PASSED is not a number", p.name));
        // The script says "A floor of 0 is not a gate" in a comment; this is
        // the part that makes it true.
        assert!(min_passed >= 1, "profile '{}' MIN_PASSED must be >= 1", p.name);

        assert!(
            p.max_skip == "any" || p.max_skip.parse::<u32>().is_ok(),
            "profile '{}' MAX_SKIP must be a number or 'any', got {:?}",
            p.name,
            p.max_skip
        );
        assert!(
            matches!(p.os.as_str(), "any" | "Linux" | "Darwin"),
            "profile '{}' has an unknown os {:?}",
            p.name,
            p.os
        );
    }
}

/// **The drift check.** Every `KASTELLAN_*_REQUIRE_E2E` the script sets must be
/// a knob something actually reads.
///
/// Renaming either half used to leave a profile setting a variable nothing
/// reads: the precondition reverts to skip-as-pass and its `[E2E]` lines simply
/// stop appearing. With per-tier floors that is now caught at run time on a
/// staged host — but only there, and only by whoever next runs the gate.
#[test]
fn every_knob_the_script_sets_is_one_the_tree_reads() {
    let src = gate_script();
    let known: Vec<&str> = KNOBS.iter().map(RequireKnob::env).collect();

    let mut seen = 0usize;
    for token in src.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if !token.starts_with("KASTELLAN_") || !token.ends_with("_REQUIRE_E2E") {
            continue;
        }
        seen += 1;
        assert!(
            known.contains(&token),
            "{} names {token}, which no RequireKnob reads. Either the const was renamed \
             and the script not updated, or a new tier's knob is missing from \
             require::KNOBS. Known: {known:?}",
            script_path().display()
        );
    }
    assert!(
        seen >= known.len(),
        "found only {seen} knob mentions in the gate script — a scan that matches nothing \
         cannot fail, so this floor is what stops the check above being vacuous"
    );
}

/// ...and the other direction: a knob nothing gates is a knob nobody sets.
///
/// This is what stops the next tier landing with a `RequireKnob` and no profile
/// — which is how a tier ends up "gated" by a variable that exists but is never
/// demanded by anything an operator runs.
#[test]
fn every_knob_the_tree_defines_is_set_by_some_profile() {
    let all_knobs: String =
        profiles().iter().map(|p| p.knobs.clone()).collect::<Vec<_>>().join(" ");
    for knob in KNOBS {
        assert!(
            all_knobs.contains(knob.env()),
            "no gate profile sets {} ({}), so nothing demands that tier. Add it to a \
             profile in scripts/run-e2e-gate.sh, or say in the script why it cannot be \
             gated yet — as the sandbox and container tiers do.",
            knob.env(),
            knob.tier()
        );
    }
}

/// Every `[E2E]` floor names a real tier phrase, and demands at least one line.
///
/// A floor keyed on a misspelled tier counts `[E2E] <nothing>:` forever — zero,
/// always, on every host. That is the `microvm_roundtrip_e2e` shape (a name
/// that matches nothing) moved from the suite selector into the assertion, and
/// it fails closed rather than open, which makes it a gate that cries wolf
/// instead of one that sleeps. Both are worth catching here.
#[test]
fn every_e2e_floor_names_a_real_tier_and_demands_at_least_one_line() {
    let tiers: Vec<&str> = KNOBS.iter().map(RequireKnob::tier).collect();
    let mut floors_seen = 0usize;

    for p in profiles() {
        assert!(!p.e2e_floors.is_empty(), "profile '{}' names no E2E floor", p.name);
        for pair in p.e2e_floors.split(',') {
            let (tier, floor) = pair
                .split_once('=')
                .unwrap_or_else(|| panic!("profile '{}' floor {pair:?} is not tier=N", p.name));
            assert!(
                tiers.contains(&tier),
                "profile '{}' demands evidence from tier {tier:?}, which no RequireKnob \
                 announces — it would count 0 on every host. Known tiers: {tiers:?}",
                p.name
            );
            let n: u32 = floor
                .parse()
                .unwrap_or_else(|_| panic!("profile '{}' floor for {tier} is not a number", p.name));
            assert!(n >= 1, "profile '{}' floor for {tier} must be >= 1", p.name);
            floors_seen += 1;
        }
    }
    assert!(floors_seen >= 4, "only {floors_seen} floors parsed — the table shape changed");
}

/// The `guard-tier` profile sets every knob its `bootstrap` consults.
///
/// `bootstrap()` reads its `action` from the guard knob but its first three
/// preconditions answer to `KASTELLAN_PG_REQUIRE_E2E` and
/// `KASTELLAN_SANDBOX_REQUIRE_E2E`. Setting only the guard knob therefore still
/// lets an unreachable supervisor return `None` and report every guard-tier
/// test green — #622's own shape, under a demanded knob. The profile setting
/// all three is what makes the documented command sound, so it is worth
/// pinning rather than leaving to whoever next edits the row.
#[test]
fn the_guard_tier_profile_sets_every_knob_its_bootstrap_consults() {
    let guard = profiles()
        .into_iter()
        .find(|p| p.name == "guard-tier")
        .expect("the guard-tier profile exists");
    for required in
        ["KASTELLAN_GUARD_REQUIRE_E2E", "KASTELLAN_PG_REQUIRE_E2E", "KASTELLAN_SANDBOX_REQUIRE_E2E"]
    {
        assert!(
            guard.knobs.contains(required),
            "the guard-tier profile must set {required}: bootstrap()'s preconditions span \
             three knobs, and a partial demand reports green (#622)"
        );
    }
}
