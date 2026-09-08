//! Unit tests for [`super::container`] — the macOS Apple-`container` tier's
//! preconditions and its image-age rule (#684, #687).
//!
//! Deliberately **not** cfg-gated to macOS. The decision logic is pure, so
//! both hosts compile and run it; only the functions that shell out to the
//! `container` CLI are `#[cfg(target_os = "macos")]`. That mirrors the split
//! `microvm/mod.rs` documents for Firecracker, and for the same reason: a
//! `cfg`-gated rule is verified by one host's run only, and the two facts most
//! worth protecting here — the exact-reference match and the staleness
//! ordering — need no VM to check.

use std::path::PathBuf;

use super::container::{
    cli_unavailable_reason, container_preflight, image_age, image_missing_reason,
    stale_image_reason, unverified_age_reason, ContainerImage, ImageAge, SourceStamp,
    PYTHON_EXEC_BUILD_SCRIPT,
};

/// Record what a preflight step was asked, so the ORDER and the
/// SHORT-CIRCUIT are assertable rather than merely plausible.
#[derive(Default)]
struct Calls {
    log: std::cell::RefCell<Vec<&'static str>>,
}

impl Calls {
    fn note(&self, what: &'static str) {
        self.log.borrow_mut().push(what);
    }
    fn seen(&self) -> Vec<&'static str> {
        self.log.borrow().clone()
    }
}

/// 2026-06-26T03:54:57Z, the measured build time of this Mac's image.
const IMAGE_BUILT: i64 = 1_782_446_097;
/// 2026-09-03T19:15:00Z, ~the measured mtime of the newest worker source.
const SOURCE_NEWER: i64 = 1_788_549_300;
/// 2026-06-25T21:49:00Z, ~the measured mtime of the Containerfile.
const SOURCE_OLDER: i64 = 1_782_424_140;

fn stamp(path: &str, modified_unix: i64) -> SourceStamp {
    SourceStamp {
        path: PathBuf::from(path),
        modified_unix,
    }
}

// ---------------------------------------------------------------------------
// image_age — #687's staleness rule
// ---------------------------------------------------------------------------

#[test]
fn an_image_older_than_a_source_is_stale() {
    // The live case on this Mac: the image predates the security audit.
    let age = image_age(
        Some(IMAGE_BUILT),
        &[
            stamp("workers/python-exec/Containerfile", SOURCE_OLDER),
            stamp("workers/python-exec/src/exec/mod.rs", SOURCE_NEWER),
        ],
        &[],
    );
    match age {
        ImageAge::Stale { built_unix, newest } => {
            assert_eq!(built_unix, IMAGE_BUILT);
            assert_eq!(
                newest.path,
                PathBuf::from("workers/python-exec/src/exec/mod.rs"),
                "the verdict must name the NEWEST source, the one that proves the point"
            );
        }
        other => panic!("expected Stale, got {other:?}"),
    }
}

#[test]
fn an_image_newer_than_every_source_is_not_certified_fresh() {
    // The one-sidedness is the design. The fresh arm is NewerThanSources, and
    // this test exists so a later "simplification" to a `Fresh` bool has to
    // delete an assertion that says why it must not.
    let age = image_age(
        Some(SOURCE_NEWER),
        &[stamp("workers/python-exec/src/exec/mod.rs", IMAGE_BUILT)],
        &[],
    );
    assert_eq!(age, ImageAge::NewerThanSources { unstat: vec![] });
}

#[test]
fn a_source_with_the_same_mtime_as_the_image_is_not_stale() {
    // Boundary: equal timestamps mean the image was built in the same second
    // the source was written. Calling that stale would cry wolf on a rebuild
    // that immediately follows an edit — the failure #667's own design names.
    let age = image_age(
        Some(IMAGE_BUILT),
        &[stamp("workers/python-exec/src/main.rs", IMAGE_BUILT)],
        &[],
    );
    assert_eq!(age, ImageAge::NewerThanSources { unstat: vec![] });
}

#[test]
fn a_stale_source_wins_over_an_unstat_able_one() {
    // Positive evidence of staleness outranks incomplete coverage: if ANY
    // source is newer, the image cannot contain it, whatever else we failed
    // to read.
    let age = image_age(
        Some(IMAGE_BUILT),
        &[stamp("workers/python-exec/src/exec/mod.rs", SOURCE_NEWER)],
        &[PathBuf::from("Cargo.lock")],
    );
    assert!(
        matches!(age, ImageAge::Stale { .. }),
        "an unreadable source must not downgrade positive staleness evidence"
    );
}

#[test]
fn unstat_able_sources_are_carried_into_the_fresh_arm() {
    // #680's review found `Fresh` certifying on partial evidence. The fresh
    // arm must carry what it could NOT check so the caller can warn.
    let age = image_age(
        Some(SOURCE_NEWER),
        &[stamp("workers/python-exec/src/main.rs", IMAGE_BUILT)],
        &[PathBuf::from("Cargo.lock")],
    );
    assert_eq!(
        age,
        ImageAge::NewerThanSources {
            unstat: vec![PathBuf::from("Cargo.lock")]
        }
    );
}

#[test]
fn an_image_with_no_timestamp_is_indeterminate() {
    let age = image_age(None, &[stamp("workers/python-exec/src/main.rs", IMAGE_BUILT)], &[]);
    assert!(
        matches!(age, ImageAge::Indeterminate { .. }),
        "a missing image timestamp must not read as fresh"
    );
}

#[test]
fn no_readable_source_at_all_is_indeterminate_not_fresh() {
    // The vacuity guard. With an empty source list the "newer than every
    // source" test is vacuously true, which would certify any image on a host
    // where the enumeration silently returned nothing — #683's "green whether
    // the loop found nothing or never ran".
    let age = image_age(Some(IMAGE_BUILT), &[], &[PathBuf::from("Cargo.lock")]);
    assert!(
        matches!(age, ImageAge::Indeterminate { .. }),
        "zero comparable sources must not be a pass"
    );
}

// ---------------------------------------------------------------------------
// container_preflight — #684's folding defect, and the gate order
// ---------------------------------------------------------------------------

/// The reference the three container suites actually ask for.
const PY_IMAGE: &str = "kastellan/python-exec:dev";

#[test]
fn a_failed_cli_spawn_is_not_reported_as_a_missing_image() {
    // THE #684 DEFECT, as a test. The shipped helpers folded an Err from
    // spawning `container` into "image not present", so an operator whose
    // system service was down was sent to build-image.sh — a build cannot
    // start a service. The two must reach DIFFERENT reasons.
    let seen = std::cell::RefCell::new(String::new());
    container_preflight(
        PY_IMAGE,
        || Err("container system service is not running".to_string()),
        || panic!("the image must not be inspected once the CLI is unusable"),
        |_, _| panic!("the age must not be consulted once the CLI is unusable"),
        |reason| {
            *seen.borrow_mut() = reason.to_string();
            true
        },
        |_| panic!("a CLI fault is not a warn-and-run"),
    );
    let reason = seen.borrow().clone();
    assert!(
        !reason.contains("build-image.sh"),
        "an unusable CLI must NOT send the operator to the image build: {reason}"
    );
    assert!(
        reason.contains("container system start"),
        "it must name the remedy that actually applies: {reason}"
    );
}

#[test]
fn an_absent_image_sends_the_operator_to_the_build_script() {
    let seen = std::cell::RefCell::new(String::new());
    container_preflight(
        PY_IMAGE,
        || Ok(()),
        || Ok(None),
        |_, _| panic!("the age must not be consulted when the image is absent"),
        |reason| {
            *seen.borrow_mut() = reason.to_string();
            true
        },
        |_| panic!("an absent image is not a warn-and-run"),
    );
    let reason = seen.borrow().clone();
    assert!(
        reason.contains("scripts/workers/python-exec/build-image.sh"),
        "an absent image must name the script that builds it: {reason}"
    );
}

#[test]
fn a_broken_image_inspect_is_not_reported_as_a_missing_image() {
    // The second half of the folding defect: the inspect itself can fail to
    // parse (a missing plugin prints a human-readable error and still exits
    // 0 — measured). That is a CLI fault, not an absent image.
    let seen = std::cell::RefCell::new(String::new());
    container_preflight(
        PY_IMAGE,
        || Ok(()),
        || Err("inspect output is not JSON".to_string()),
        |_, _| panic!("the age must not be consulted when the inspect failed"),
        |reason| {
            *seen.borrow_mut() = reason.to_string();
            true
        },
        |_| panic!("a broken inspect is not a warn-and-run"),
    );
    let reason = seen.borrow().clone();
    assert!(
        !reason.contains("build-image.sh"),
        "a broken inspect must not be reported as an absent image: {reason}"
    );
}

#[test]
fn the_cli_is_probed_before_the_image_is_inspected() {
    let calls = Calls::default();
    container_preflight(
        PY_IMAGE,
        || {
            calls.note("probe");
            Ok(())
        },
        || {
            calls.note("inspect");
            Ok(Some(ContainerImage {
                reference: PY_IMAGE.to_string(),
                created_unix: Some(SOURCE_NEWER),
            }))
        },
        |_, _| {
            calls.note("age");
            ImageAge::NewerThanSources { unstat: vec![] }
        },
        |_| panic!("everything was met"),
        |_| panic!("nothing to warn about"),
    );
    assert_eq!(
        calls.seen(),
        vec!["probe", "inspect", "age"],
        "the gates must run in the order an operator can act on"
    );
}

#[test]
fn a_met_preflight_runs_the_test() {
    let skipped = container_preflight(
        PY_IMAGE,
        || Ok(()),
        || {
            Ok(Some(ContainerImage {
                reference: PY_IMAGE.to_string(),
                created_unix: Some(SOURCE_NEWER),
            }))
        },
        |_, _| ImageAge::NewerThanSources { unstat: vec![] },
        |_| panic!("everything was met"),
        |_| panic!("nothing to warn about"),
    );
    assert!(!skipped, "a fully met preflight must not skip");
}

#[test]
fn a_stale_image_is_an_unmet_precondition_not_a_warning() {
    // The operator's decision (2026-09-09): a stale image [SKIP]s a plain
    // `cargo test` and PANICS under KASTELLAN_MICROVM_REQUIRE_E2E. Routing it
    // through `unmet` is what gives it both behaviours at once; routing it
    // through `warn` would run the suite against code that is not there.
    let to_unmet = std::cell::RefCell::new(String::new());
    let skipped = container_preflight(
        PY_IMAGE,
        || Ok(()),
        || {
            Ok(Some(ContainerImage {
                reference: PY_IMAGE.to_string(),
                created_unix: Some(IMAGE_BUILT),
            }))
        },
        |_, _| ImageAge::Stale {
            built_unix: IMAGE_BUILT,
            newest: stamp("workers/python-exec/src/exec/mod.rs", SOURCE_NEWER),
        },
        |reason| {
            *to_unmet.borrow_mut() = reason.to_string();
            true
        },
        |_| panic!("a stale image must not be a warn-and-run"),
    );
    assert!(skipped, "a stale image must stop the test");
    let reason = to_unmet.borrow().clone();
    assert!(
        reason.contains("build-image.sh"),
        "the stale reason must name the remedy: {reason}"
    );
}

#[test]
fn an_indeterminate_age_warns_and_still_runs() {
    // Absence of a comparable timestamp is not evidence of staleness;
    // downgrading a real VM run to a skip would lose coverage for nothing.
    // Same treatment #667 gives its Indeterminate arm.
    let warned = std::cell::RefCell::new(String::new());
    let skipped = container_preflight(
        PY_IMAGE,
        || Ok(()),
        || {
            Ok(Some(ContainerImage {
                reference: PY_IMAGE.to_string(),
                created_unix: None,
            }))
        },
        |_, _| ImageAge::Indeterminate {
            detail: "no build timestamp".to_string(),
        },
        |_| panic!("an indeterminate age is not an unmet precondition"),
        |reason| {
            *warned.borrow_mut() = reason.to_string();
            false
        },
    );
    assert!(!skipped, "an indeterminate age must still run the test");
    assert!(!warned.borrow().is_empty(), "it must say what it could not establish");
}

#[test]
fn an_unverified_source_warns_even_though_the_image_is_newer() {
    // #680's review found `Fresh` certifying on partial evidence. A source
    // that could not be stat'd did not participate in the comparison, so the
    // caller must be told rather than handed a clean bill of health.
    let warned = std::cell::RefCell::new(String::new());
    let skipped = container_preflight(
        PY_IMAGE,
        || Ok(()),
        || {
            Ok(Some(ContainerImage {
                reference: PY_IMAGE.to_string(),
                created_unix: Some(SOURCE_NEWER),
            }))
        },
        |_, _| ImageAge::NewerThanSources {
            unstat: vec![PathBuf::from("Cargo.lock")],
        },
        |_| panic!("an unverified source is not an unmet precondition"),
        |reason| {
            *warned.borrow_mut() = reason.to_string();
            false
        },
    );
    assert!(!skipped);
    assert!(
        warned.borrow().contains("Cargo.lock"),
        "the warning must name what went unchecked: {}",
        warned.borrow()
    );
}

// ---------------------------------------------------------------------------
// The reason renderers
// ---------------------------------------------------------------------------

#[test]
fn the_stale_reason_names_the_image_the_source_and_the_remedy() {
    let reason = stale_image_reason(
        PY_IMAGE,
        IMAGE_BUILT,
        &stamp("workers/python-exec/src/exec/mod.rs", SOURCE_NEWER),
        PYTHON_EXEC_BUILD_SCRIPT,
    );
    assert!(reason.contains(PY_IMAGE), "name the image: {reason}");
    assert!(
        reason.contains("workers/python-exec/src/exec/mod.rs"),
        "name the source that proves it: {reason}"
    );
    assert!(
        reason.contains("scripts/workers/python-exec/build-image.sh"),
        "name the remedy: {reason}"
    );
}

#[test]
fn every_reason_is_a_single_line() {
    // `grep -c '^[SKIP]'` over a --nocapture sweep is how a green run is
    // audited here, and `skip_line` flattens only what it is given. A reason
    // carrying its own newline would emit an orphan continuation line that no
    // audit could attribute — the shape `probe_reason` was fixed for.
    let reasons = [
        cli_unavailable_reason("service down"),
        image_missing_reason(PY_IMAGE),
        stale_image_reason(PY_IMAGE, IMAGE_BUILT, &stamp("a/b.rs", SOURCE_NEWER), PYTHON_EXEC_BUILD_SCRIPT),
        unverified_age_reason(PY_IMAGE, &[PathBuf::from("Cargo.lock")]),
        unverified_age_reason(PY_IMAGE, &[]),
    ];
    for reason in reasons {
        assert!(!reason.contains('\n'), "reason must be one line: {reason}");
        assert!(!reason.is_empty(), "reason must say something");
    }
}

