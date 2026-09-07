//! Unit tests for the pure freshness verdict and its operator-facing wording.
//!
//! Split out of `freshness.rs` when the #680 review's fixes pushed the pair
//! over the tree's 500-line guideline. Every test here runs on any host —
//! that is the whole reason the decision is pure.

use super::*;

/// Two digests that are definitely different, without pretending to be
/// real sha256 output: only equality is under test.
const A: &str = "aaaa";
const B: &str = "bbbb";

/// A fully-comparable pair.
fn d(name: &str, in_image: &str, in_target: &str) -> BakedDigest {
    BakedDigest {
        name: name.to_string(),
        in_image: Ok(in_image.to_string()),
        in_target: Ok(in_target.to_string()),
    }
}

/// A binary the working tree has not built.
fn unbuilt(name: &str, in_image: Option<&str>) -> BakedDigest {
    BakedDigest {
        name: name.to_string(),
        in_image: in_image.map(str::to_string).ok_or(Missing::NoImageReader),
        in_target: Err(Missing::NotBuilt),
    }
}

/// A binary no image on this host can be read for — the benign image-side
/// cause.
fn no_reader(name: &str) -> BakedDigest {
    BakedDigest {
        name: name.to_string(),
        in_image: Err(Missing::NoImageReader),
        in_target: Ok(A.to_string()),
    }
}

/// A binary this specific image would not yield — the NON-benign cause.
fn unreadable(name: &str, detail: &str) -> BakedDigest {
    BakedDigest {
        name: name.to_string(),
        in_image: Err(Missing::Unreadable { detail: detail.to_string() }),
        in_target: Ok(A.to_string()),
    }
}

#[test]
fn matching_digests_are_fresh_and_fully_verified() {
    let verdict = freshness(&[d("init", A, A), d("worker", B, B)]);
    assert_eq!(verdict, Freshness::Fresh { unverified: vec![] });
}

/// The whole point: the image holds something other than what we build.
#[test]
fn a_differing_digest_is_stale_and_names_the_binary() {
    assert_eq!(freshness(&[d("init", A, B)]), Freshness::Stale { binary: "init".to_string() });
}

/// One mismatch is enough. A guest-init change leaves each image's
/// worker untouched, so this is the realistic shape, not a corner case.
#[test]
fn one_differing_binary_is_enough_when_the_other_matches() {
    let verdict = freshness(&[d("worker", A, A), d("init", A, B)]);
    assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
}

/// The regression that motivated dropping mtimes: cargo relinks an
/// unchanged binary and moves its mtime. Identical content is FRESH
/// however the timestamps sit, and six DGX images depended on this.
#[test]
fn identical_content_is_fresh_regardless_of_any_timestamp() {
    // No timestamp is even an input here — that is the assertion.
    assert_eq!(freshness(&[d("init", A, A)]), Freshness::Fresh { unverified: vec![] });
}

// ---------------------------------------------------------------
// #680 review, finding 1: a partially-comparable image used to be
// certified Fresh in SILENCE. Seven of the eight images bake an init
// AND a worker; the init is stable and the worker is what goes stale.
// ---------------------------------------------------------------

/// The load-bearing regression test for that finding. A matching init
/// must NOT certify an unverifiable worker — the verdict may still run,
/// but it must carry the worker so the caller can warn.
#[test]
fn a_matching_init_does_not_certify_an_unverifiable_worker() {
    let verdict = freshness(&[d("init", A, A), unbuilt("worker", Some(B))]);
    assert_eq!(
        verdict,
        Freshness::Fresh {
            unverified: vec![Unverified {
                binary: "worker".to_string(),
                why: Missing::NotBuilt
            }]
        },
        "a matching init must not vouch for a worker nothing compared"
    );
}

/// ...and the caveat must say which binary and why, because "build it"
/// and "install e2fsprogs" are different remedies.
#[test]
fn a_partly_verified_image_says_which_binary_was_not_checked() {
    let msg = unverified_reason(
        "kv-demo.ext4",
        &[Unverified { binary: "worker".to_string(), why: Missing::NotBuilt }],
        true,
    );
    assert!(msg.contains("worker"), "must name the binary: {msg}");
    assert!(msg.contains("PARTLY gated"), "must say the gate was partial: {msg}");
    assert!(msg.contains(RELEASE_BUILD_SCRIPT), "must give the remedy: {msg}");
}

/// The two leads must not read alike: "partly gated" and "not gated"
/// are different facts and an operator acts on them differently.
#[test]
fn a_partly_gated_run_reads_differently_from_an_ungated_one() {
    let u = [Unverified { binary: "worker".to_string(), why: Missing::NotBuilt }];
    let partly = unverified_reason("kv-demo.ext4", &u, true);
    let ungated = unverified_reason("kv-demo.ext4", &u, false);
    assert_ne!(partly, ungated);
    assert!(partly.contains("PARTLY gated"), "{partly}");
    assert!(ungated.contains("NOT gated"), "{ungated}");
}

// ---------------------------------------------------------------
// #680 review, finding 2: every image-read failure blamed e2fsprogs,
// and three of the four then booted the VM anyway.
// ---------------------------------------------------------------

/// An image that a working reader cannot read is positive evidence
/// about THIS image, so it must not land in the benign bucket.
#[test]
fn an_image_a_working_reader_cannot_read_is_unusable_not_indeterminate() {
    let verdict = freshness(&[unreadable("init", "Filesystem not open")]);
    assert_eq!(
        verdict,
        Freshness::Unusable {
            binary: "init".to_string(),
            detail: "Filesystem not open".to_string()
        }
    );
}

/// ...while an absent reader is benign: it says nothing about any image.
#[test]
fn an_absent_reader_stays_benign() {
    let verdict = freshness(&[no_reader("init")]);
    assert_eq!(
        verdict,
        Freshness::Indeterminate {
            unverified: vec![Unverified {
                binary: "init".to_string(),
                why: Missing::NoImageReader
            }]
        }
    );
}

/// The distinction must survive into the wording, or it buys nothing:
/// the reader's own words must reach the operator instead of a guess.
#[test]
fn the_unusable_reason_carries_the_readers_own_words() {
    let msg = unusable_reason(
        "kv-demo.ext4",
        "kastellan-microvm-init",
        "Filesystem not open",
        Some("scripts/x.sh"),
    );
    assert!(msg.contains("Filesystem not open"), "must carry the cause: {msg}");
    assert!(!msg.contains("e2fsprogs"), "must not blame the wrong thing: {msg}");
    assert!(msg.contains("bash scripts/x.sh"), "must give the rebuild: {msg}");
    assert!(msg.contains("#667"), "must be traceable: {msg}");
}

/// A real mismatch outranks an unreadable sibling: it is the more
/// specific diagnosis and both have the same remedy.
#[test]
fn a_real_mismatch_outranks_an_unreadable_sibling() {
    let verdict = freshness(&[unreadable("worker", "boom"), d("init", A, B)]);
    assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
}

/// ...and an unreadable image outranks a merely-unbuilt reference,
/// because only one of the two says anything about the image.
#[test]
fn an_unreadable_image_outranks_an_unbuilt_reference() {
    let verdict = freshness(&[unbuilt("worker", None), unreadable("init", "boom")]);
    assert!(matches!(verdict, Freshness::Unusable { .. }), "got {verdict:?}");
}

// ---------------------------------------------------------------
// Absence of evidence
// ---------------------------------------------------------------

/// Absence of a reference is not evidence of freshness. If this ever
/// returned `Fresh`, #667 would be back: a clean bill of health for an
/// image nothing looked at.
#[test]
fn nothing_comparable_is_indeterminate_never_fresh() {
    let verdict = freshness(&[unbuilt("init", None)]);
    assert_eq!(
        verdict,
        Freshness::Indeterminate {
            unverified: vec![
                Unverified { binary: "init".to_string(), why: Missing::NoImageReader },
                Unverified { binary: "init".to_string(), why: Missing::NotBuilt },
            ]
        }
    );
}

/// An empty registry entry (an unknown image) must reach the same honest
/// answer rather than passing vacuously.
#[test]
fn an_empty_baked_list_is_indeterminate() {
    assert_eq!(freshness(&[]), Freshness::Indeterminate { unverified: vec![] });
}

/// A partially-comparable set still yields a real verdict from the pair
/// that IS comparable — dropping to Indeterminate would throw away a
/// usable signal.
#[test]
fn an_uncomparable_binary_does_not_mask_a_comparable_stale_one() {
    let verdict = freshness(&[unbuilt("worker", None), d("init", A, B)]);
    assert_eq!(verdict, Freshness::Stale { binary: "init".to_string() });
}

// ---------------------------------------------------------------
// Wording
// ---------------------------------------------------------------

#[test]
fn the_stale_reason_names_the_image_the_binary_and_both_rebuild_routes() {
    let msg = stale_reason("kv-demo.ext4", "kastellan-microvm-init", Some("scripts/x.sh"));
    assert!(msg.contains("kv-demo.ext4"), "must name the image: {msg}");
    assert!(msg.contains("kastellan-microvm-init"), "must name the binary: {msg}");
    assert!(msg.contains("bash scripts/x.sh"), "must give the per-image command: {msg}");
    assert!(msg.contains(REBUILD_ALL_SCRIPT), "must offer rebuild-all: {msg}");
    assert!(msg.contains("#667"), "must be traceable to the issue: {msg}");
}

/// `Missing::NOT_BUILT_REMEDY` has to be a `const`, so it spells the script
/// path as a literal instead of interpolating [`RELEASE_BUILD_SCRIPT`].
///
/// That is a second copy of one fact, which is the drift channel this whole
/// change exists to close — so it is pinned rather than trusted. A rename that
/// updates the const and not the literal (or the reverse) fails here instead
/// of sending an operator to a path that does not exist.
#[test]
fn the_not_built_remedy_names_the_canonical_producer() {
    let msg = unverified_reason(
        "web-fetch.ext4",
        &[Unverified { binary: "a-bin".to_string(), why: Missing::NotBuilt }],
        false,
    );
    assert!(
        msg.contains(RELEASE_BUILD_SCRIPT),
        "the not-built remedy must name {RELEASE_BUILD_SCRIPT}: {msg}"
    );
}

/// A digest mismatch has **two** possible causes, and only one of them is
/// staleness (issue #682).
///
/// Cargo's feature unification makes the bytes depend on the package
/// selection of the build that last wrote `target/release/`, so a narrow
/// `cargo build -p …` leaves a reference no image was ever built from. The
/// verdict cannot tell that apart from a genuinely stale image — there is no
/// cheap local discriminator — so the message must name the cheap check
/// FIRST. Sending an operator to rebuild eight images when one `bash
/// scripts/build-release.sh` would clear it is how a gate earns the
/// reputation that gets it switched off.
#[test]
fn the_stale_reason_offers_the_cheap_rebuild_before_the_expensive_one() {
    let msg = stale_reason("kv-demo.ext4", "kastellan-microvm-init", Some("scripts/x.sh"));
    assert!(msg.contains("#682"), "must be traceable to the issue: {msg}");
    assert!(
        msg.contains(&format!("bash {RELEASE_BUILD_SCRIPT}")),
        "must name the canonical producer: {msg}"
    );

    // Order is the point, not mere presence: an operator who reads one clause
    // and acts must be sent to the 3-second command, not the 8-image one.
    let cheap = msg.find(RELEASE_BUILD_SCRIPT).expect("checked above");
    let expensive = msg.find("scripts/x.sh").expect("the per-image script is named");
    assert!(
        cheap < expensive,
        "the cheap check must come before the image rebuild: {msg}"
    );
}

/// The invocation-skew caveat belongs to a digest MISMATCH and to nothing
/// else.
///
/// [`Freshness::Unusable`] means a working reader could not get the binary
/// out of the image, so no digest was ever compared and re-running a build
/// cannot change the outcome. Offering the remedy there would be a confident
/// wrong answer — the failure this module's four verdicts exist to prevent.
#[test]
fn the_unusable_reason_does_not_blame_the_build_selection() {
    let msg = unusable_reason("kv-demo.ext4", "some-bin", "File not found", Some("scripts/x.sh"));
    assert!(!msg.contains("#682"), "no digest was compared, so skew is irrelevant: {msg}");
    assert!(msg.contains("File not found"), "must carry the reader's words: {msg}");
}

/// An unknown image must not have a build script invented for it — the
/// hint would send the operator to a path that does not exist, which is
/// the failure `images.rs` was written to prevent.
#[test]
fn the_stale_reason_invents_no_script_for_an_unregistered_image() {
    let msg = stale_reason("mystery.ext4", "some-bin", None);
    assert!(msg.contains("no per-image script recorded"), "must admit it: {msg}");
    assert!(msg.contains(REBUILD_ALL_SCRIPT), "must still offer a route: {msg}");
}

/// The three causes need three different remedies, so they must not
/// render as one generic "could not check".
#[test]
fn the_unverified_reason_separates_the_causes() {
    let unbuilt_msg = unverified_reason(
        "web-fetch.ext4",
        &[Unverified { binary: "a-bin".to_string(), why: Missing::NotBuilt }],
        false,
    );
    assert!(
        unbuilt_msg.contains(RELEASE_BUILD_SCRIPT),
        "how to fix, via the one producer every image is baked from: {unbuilt_msg}"
    );
    assert!(!unbuilt_msg.contains("e2fsprogs"), "wrong blame: {unbuilt_msg}");

    let no_tool = unverified_reason(
        "web-fetch.ext4",
        &[Unverified { binary: "a-bin".to_string(), why: Missing::NoImageReader }],
        false,
    );
    assert!(no_tool.contains("e2fsprogs"), "must name the tool: {no_tool}");
    assert!(!no_tool.contains("cargo build"), "wrong blame: {no_tool}");
}

/// Several binaries failing for the SAME reason must read as one clause,
/// not one clause each — a whole-image failure is one fact.
#[test]
fn the_unverified_reason_groups_binaries_sharing_a_cause() {
    let msg = unverified_reason(
        "web-fetch.ext4",
        &[
            Unverified { binary: "one".to_string(), why: Missing::NotBuilt },
            Unverified { binary: "two".to_string(), why: Missing::NotBuilt },
        ],
        false,
    );
    assert!(msg.contains("one, two"), "must group: {msg}");
    assert_eq!(msg.matches(RELEASE_BUILD_SCRIPT).count(), 1, "one remedy, once: {msg}");
}

/// ...but two DIFFERENT causes must stay two clauses.
#[test]
fn the_unverified_reason_keeps_distinct_causes_apart() {
    let msg = unverified_reason(
        "web-fetch.ext4",
        &[
            Unverified { binary: "one".to_string(), why: Missing::NotBuilt },
            Unverified { binary: "two".to_string(), why: Missing::NoImageReader },
        ],
        false,
    );
    assert!(msg.contains(RELEASE_BUILD_SCRIPT), "{msg}");
    assert!(msg.contains("e2fsprogs"), "{msg}");
    assert!(msg.contains("; "), "must be two clauses: {msg}");
}

/// The unregistered-image case has a different cause and so gets a
/// different sentence: nothing knows what to look for, as opposed to
/// having looked and found nothing.
#[test]
fn the_unverified_reason_distinguishes_an_unregistered_image() {
    let msg = unverified_reason("mystery.ext4", &[], false);
    assert!(msg.contains("not in the rootfs registry"), "must name the cause: {msg}");
    assert!(msg.contains("NOT gated"), "must still say the run is ungated: {msg}");
}

/// `Unreadable` is the one cause that arises on BOTH ends — a corrupt image,
/// or a permissions error on `target/release` — so its heading must not
/// claim the image. A target-side failure blamed on the image sends the
/// operator to rebuild a rootfs that is fine.
#[test]
fn an_unreadable_target_binary_is_not_blamed_on_the_image() {
    let msg = unverified_reason(
        "kv-demo.ext4",
        &[Unverified {
            binary: "worker".to_string(),
            why: Missing::Unreadable { detail: "\"target/release/worker\": Permission denied".to_string() },
        }],
        false,
    );
    assert!(msg.contains("Permission denied"), "must carry the real cause: {msg}");
    assert!(!msg.contains("out of the image"), "must not blame the image: {msg}");
    assert!(!msg.contains("cargo build --release"), "it IS built; do not say otherwise: {msg}");
}

/// ...and a permissions error must not masquerade as "not built", which
/// sends the operator to a command that succeeds and changes nothing.
#[test]
fn an_unreadable_target_binary_is_not_reported_as_unbuilt() {
    let verdict = freshness(&[BakedDigest {
        name: "worker".to_string(),
        in_image: Ok(A.to_string()),
        in_target: Err(Missing::Unreadable { detail: "Permission denied".to_string() }),
    }]);
    let Freshness::Indeterminate { unverified } = verdict else {
        panic!("expected Indeterminate, got {verdict:?}")
    };
    assert_eq!(unverified.len(), 1);
    assert!(
        matches!(unverified[0].why, Missing::Unreadable { .. }),
        "must stay Unreadable, not NotBuilt: {:?}",
        unverified[0]
    );
}
