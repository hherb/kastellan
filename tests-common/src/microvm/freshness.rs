//! Does this rootfs image actually contain the code under test? (issue #667)
//!
//! # The failure this exists to stop
//!
//! `kv_demo_firecracker_persistent_e2e` once failed with
//!
//! ```text
//! persistent ext4 store must survive a VM respawn (SIGKILL + reboot); got {"value":null}
//! ```
//!
//! which reads exactly like a regression in the persistent-store path. It was
//! not: `/var/lib/kastellan/microvm/kv-demo.ext4` had been built in June, and
//! rebuilding it made the test pass unchanged.
//!
//! Every image bakes its own copy of `kastellan-microvm-init` and of its
//! worker (see [`images`](mod@super::images)), so a guest-side change is
//! invisible to the Firecracker e2es until the affected image is rebuilt.
//! That makes a stale image the micro-VM twin of the stale `target/release`
//! launcher and of the `.venv` that silently belonged to another OS: **a
//! fixture whose staleness is invisible turns a gate into a formality.** The
//! owed gate for audit item W-2 could have run start to finish against stale
//! images and reported green having tested none of it.
//!
//! # Why this compares CONTENT and not mtimes
//!
//! The issue proposed comparing the image's mtime against the binary's, and
//! that was measured on the DGX before it was written. It does not survive
//! contact:
//!
//! ```text
//! target/release/kastellan-microvm-init   2026-09-05 19:08
//! kv-demo.ext4, web-fetch.ext4, matrix.ext4,
//! net-demo.ext4, web-search.ext4, web-research.ext4   2026-09-05 14:26-14:28
//! ```
//!
//! Six images ~4h40m "stale" — and the init binary **inside every one of
//! them is byte-identical** to the one in `target/release` (verified by
//! sha256). Cargo had relinked an unchanged binary, moving its mtime without
//! changing a byte. An mtime rule would have refused six correct images on the
//! one host this check exists for, and a check that cries wolf on the common
//! case is a check somebody switches off.
//!
//! Comparing digests removes the whole class. It is also strictly stronger:
//! it catches an image built from a *stale checkout*, which the issue
//! explicitly conceded mtime could not.
//!
//! # Why the verdict is a value and not a `bool`
//!
//! Four outcomes need four different operator responses. Collapsing any of
//! them into a neighbour is what would make this module dishonest, and the
//! #680 review found two such collapses in the first version:
//!
//! * **A partially-comparable image reported [`Freshness::Fresh`] in
//!   silence.** Seven of the eight images bake an init *and* a worker; the
//!   init is stable and the worker is exactly what goes stale. One matching
//!   init was enough to certify the whole image, so an unbuilt or
//!   unreadable worker booted a June binary under a clean bill of health —
//!   #667 restored for the worker half of every image. `Fresh` now carries
//!   what it could **not** check, and a non-empty list is a `[WARN]`.
//! * **Every image-read failure blamed a missing `e2fsprogs`.** Measured on
//!   the DGX against real images: a corrupt image, an image that is not
//!   ext4, and a path absent from the image all exit **0** with empty
//!   output, exactly as a missing `debugfs` does. All four rendered as
//!   "install e2fsprogs" on a host that has it — and the first three then
//!   **booted the VM anyway**. [`Missing`] now separates the two benign
//!   causes from the one that is positive evidence.

/// Why one side's digest could not be established.
///
/// The split is the fix for the #680 review's second finding: three of these
/// four situations used to render as one sentence naming the wrong remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// The binary is absent from `target/release/`.
    ///
    /// Benign: an operator who built only some `-p` targets has nothing to
    /// compare against, which says nothing about the image.
    NotBuilt,
    /// No `debugfs`, so **no** image on this host can be read.
    ///
    /// Benign for the same reason: absence of a tool is absence of evidence.
    /// Distinguished from [`Missing::Unreadable`] structurally (an
    /// `ErrorKind::NotFound` spawning the tool), never by parsing output.
    NoImageReader,
    /// The reader ran and could not produce this file. Carries its own words.
    ///
    /// **Not benign, and the asymmetry is the point.** The registry says this
    /// image bakes this path, and
    /// `the_table_and_the_scripts_agree_on_every_in_image_destination` pins
    /// that the build script installs it there. So a host that *can* read
    /// images and cannot read *this* path is looking at something that is not
    /// the image the table describes — a truncated rebuild (the build scripts
    /// `mkfs.ext4` in place, with no temp-then-rename, so an interrupted one
    /// leaves a partial file at the final path), a corrupt image, or a layout
    /// this tree no longer produces.
    Unreadable {
        /// What the reader said, for the operator. Never matched on.
        detail: String,
    },
}

impl Missing {
    /// What to run when a binary is absent from `target/release/`.
    ///
    /// A `const` because [`Missing::remedy`] returns a *borrow*, so this arm
    /// cannot build its string with `format!` — and `concat!` takes only
    /// literals, not a const item, so `super::RELEASE_BUILD_SCRIPT` cannot be
    /// spliced in either. That leaves the script path written out a second
    /// time. It is a copy, not a single source of truth, so
    /// `the_not_built_remedy_names_the_canonical_producer` pins the two
    /// together rather than trusting them to stay in step.
    const NOT_BUILT_REMEDY: &'static str = "build it with bash scripts/build-release.sh";

    /// The remedy clause for this cause, without the binary names.
    fn remedy(&self) -> &str {
        match self {
            // Names the canonical producer rather than a bare
            // `cargo build --release`: the reference must come from the same
            // package selection every image is baked from, or fixing this
            // cause creates the #682 one.
            Missing::NotBuilt => Self::NOT_BUILT_REMEDY,
            Missing::NoImageReader => "needs debugfs from e2fsprogs",
            Missing::Unreadable { detail } => detail,
        }
    }

    /// The operator-facing phrase this cause groups under.
    ///
    /// [`Missing::Unreadable`] is side-NEUTRAL on purpose: it is the one
    /// cause that can arise on either end — a corrupt image, or a
    /// permissions error on `target/release` — so a heading naming the
    /// image would misattribute half of its occurrences. The `detail` names
    /// the file in that case.
    fn heading(&self) -> &'static str {
        match self {
            Missing::NotBuilt => "not built in target/release",
            Missing::NoImageReader => "could not be read out of the image",
            Missing::Unreadable { .. } => "could not be read",
        }
    }
}

/// One binary that could not be compared, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unverified {
    /// The `target/release/` filename.
    pub binary: String,
    /// Which side was missing, and for what reason.
    pub why: Missing,
}

/// One binary baked into an image: what the image holds, and what the
/// working tree currently builds.
///
/// `Err` on either side means *that* digest could not be established, and
/// carries which of [`Missing`]'s causes applies — the caller must not have
/// to guess, which is precisely what the first version made it do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakedDigest {
    /// The `target/release/` filename, for the operator-facing message.
    pub name: String,
    /// sha256 of the copy inside the image.
    pub in_image: Result<String, Missing>,
    /// sha256 of the copy in `target/release/`.
    pub in_target: Result<String, Missing>,
}

/// The verdict for one rootfs image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Every binary that could be compared matches.
    ///
    /// `unverified` names the binaries that could **not** be compared, for a
    /// benign reason. Empty means the image is fully verified and the caller
    /// stays silent; non-empty means the image may still be stale in those
    /// binaries and the caller `[WARN]`s. Folding the two together is what
    /// let one matching init certify a June worker (#680 review).
    Fresh { unverified: Vec<Unverified> },
    /// A binary in the image differs from the one the working tree builds, so
    /// booting this image would test code that no longer exists.
    Stale { binary: String },
    /// The image is not the one the registry describes: a reader that works
    /// elsewhere could not get this binary out of it.
    ///
    /// Treated like [`Freshness::Stale`] rather than like
    /// [`Freshness::Indeterminate`], because it is positive evidence about
    /// *this* image rather than absence of evidence about all of them.
    Unusable { binary: String, detail: String },
    /// Nothing could be compared, and every cause was benign.
    ///
    /// Deliberately **not** folded into [`Freshness::Fresh`]: absence of a
    /// reference is absence of evidence. An empty `unverified` here means the
    /// image is not in the registry at all, so nothing even knew what to look
    /// for — the one meaning encoded as an absence, and
    /// `every_image_bakes_the_guest_init` is what guarantees a *registered*
    /// image can never produce it.
    Indeterminate { unverified: Vec<Unverified> },
}

/// Collect the binaries whose comparison did not happen, with their causes.
///
/// The image side is reported first because it is the side that says
/// something about *this* image.
fn unverified(baked: &[BakedDigest]) -> Vec<Unverified> {
    let mut out = Vec::new();
    for b in baked {
        if let Err(why) = &b.in_image {
            out.push(Unverified { binary: b.name.clone(), why: why.clone() });
        }
        if let Err(why) = &b.in_target {
            out.push(Unverified { binary: b.name.clone(), why: why.clone() });
        }
    }
    out
}

/// Compare what an image holds against what the working tree builds.
///
/// Pure — the caller does the hashing — so every branch is unit-testable on
/// any host, including the macOS one that compiles the whole Firecracker
/// backend out.
///
/// Precedence, most-conclusive first:
///
/// 1. [`Freshness::Stale`] — a comparable pair disagrees. One mismatch is
///    enough: a guest-init change leaves each image's worker untouched, so
///    "one of two differs" is the realistic shape, not a corner case.
/// 2. [`Freshness::Unusable`] — the image could not be read where the
///    registry says it must be readable.
/// 3. [`Freshness::Fresh`] — something was compared and everything compared
///    matched, carrying whatever could not be checked.
/// 4. [`Freshness::Indeterminate`] — nothing was compared at all.
pub fn freshness(baked: &[BakedDigest]) -> Freshness {
    if let Some(b) = baked
        .iter()
        .find(|b| matches!((&b.in_image, &b.in_target), (Ok(i), Ok(t)) if i != t))
    {
        return Freshness::Stale { binary: b.name.clone() };
    }
    if let Some((name, detail)) = baked.iter().find_map(|b| match &b.in_image {
        Err(Missing::Unreadable { detail }) => Some((b.name.clone(), detail.clone())),
        _ => None,
    }) {
        return Freshness::Unusable { binary: name, detail };
    }
    // No mismatch and nothing positively wrong. That is only a clean bill of
    // health if something was actually compared — otherwise the run below
    // would certify an image nothing looked at, which is #667 restored.
    if baked.iter().any(|b| b.in_image.is_ok() && b.in_target.is_ok()) {
        return Freshness::Fresh { unverified: unverified(baked) };
    }
    Freshness::Indeterminate { unverified: unverified(baked) }
}

/// The operator-facing reason for a [`Freshness::Stale`] verdict.
///
/// Pure so the wording is testable without touching a filesystem. Names the
/// image, the binary that differs, the script that rebuilds *this* image and
/// the script that rebuilds them all — because the two build-script
/// directories are exactly what makes "rebuild everything" easy to get wrong,
/// and an operator reading this has just been told their gate is invalid and
/// wants the one command that fixes it.
pub fn stale_reason(image: &str, binary: &str, build_script: Option<&str>) -> String {
    format!(
        "{image} bakes a copy of {binary} that DIFFERS from the one this tree builds, so \
         booting it would test code that no longer exists — the gate would pass having \
         verified nothing (#667). {} {}",
        selection_skew_clause(),
        rebuild_clause(build_script)
    )
}

/// The cheap check that must be offered before a rebuild of eight images
/// (issue #682).
///
/// A digest mismatch has **two** causes and this verdict cannot tell them
/// apart. The obvious one is a stale image. The other is that cargo unifies
/// features *per invocation*, so the package selection of whichever build
/// last wrote `target/release/` changes the bytes of an otherwise identical
/// binary — measured on the DGX, `-p kastellan-microvm-init` and
/// `--workspace` differ from the same source, deterministically. Until #682
/// every `build-*-rootfs.sh` used its own narrow `-p` set while the deploy
/// path ran the workspace build, so after any deploy all eight *correct*
/// images were declared stale.
///
/// Pointing every build script at [`super::RELEASE_BUILD_SCRIPT`] removes
/// that skew for the documented workflows, but a hand-run
/// `cargo build --release -p <one worker>` still rewrites one reference. No
/// cheap local discriminator exists — the two causes are byte-identical in
/// their symptom — so the honest move is to name the three-second remedy
/// first and the eight-image one second. An operator who reads one clause
/// and acts must be sent to the cheap one; a gate that habitually demands the
/// expensive one is a gate that gets switched off, which is the failure this
/// whole module was written to avoid.
fn selection_skew_clause() -> String {
    format!(
        "CHECK THIS FIRST if you have not changed guest source: cargo unifies features per \
         invocation, so the package selection of the build that last wrote target/release/ \
         changes the BYTES of an identical binary, and a narrow `cargo build -p …` leaves a \
         reference no image is ever built from (#682). Re-run `bash {}` — it re-links from \
         cargo's cache in seconds — and try again.",
        super::RELEASE_BUILD_SCRIPT
    )
}

/// The operator-facing reason for a [`Freshness::Unusable`] verdict.
///
/// Says what the reader said rather than guessing a remedy: the first version
/// blamed a missing `e2fsprogs` for a corrupt image, on a host that had it.
pub fn unusable_reason(
    image: &str,
    binary: &str,
    detail: &str,
    build_script: Option<&str>,
) -> String {
    format!(
        "{image} could not yield its copy of {binary} on a host that CAN read images, so it \
         is not the image the registry describes — a truncated or corrupt rebuild, or a \
         layout this tree no longer produces ({detail}) (#667). {}",
        rebuild_clause(build_script)
    )
}

/// The shared "here is the one command that fixes it" tail.
///
/// One renderer because both panicking verdicts have the same remedy, and a
/// hint that drifted between them would send an operator to a path that does
/// not exist — the failure `images.rs` exists to prevent.
fn rebuild_clause(build_script: Option<&str>) -> String {
    let rebuild = match build_script {
        Some(script) => format!("bash {script}"),
        // An image outside the registry has no known script; say so rather
        // than guessing a filename.
        None => format!("bash {} (no per-image script recorded)", super::REBUILD_ALL_SCRIPT),
    };
    format!("Rebuild it: {rebuild} — or rebuild every image: bash {}", super::REBUILD_ALL_SCRIPT)
}

/// The operator-facing caveat listing what could not be checked.
///
/// Shared by the [`Freshness::Fresh`] and [`Freshness::Indeterminate`] arms,
/// because the two say the same thing about the same binaries and differ only
/// in whether anything *else* was verified. `gated` distinguishes them: a
/// partially-verified image was still gated on what could be read, an image
/// with nothing comparable was not gated at all.
///
/// Groups by cause, so "build the binary", "install e2fsprogs" and a reader's
/// own complaint stay three different remedies rather than one vague sentence.
pub fn unverified_reason(image: &str, unverified: &[Unverified], gated: bool) -> String {
    if unverified.is_empty() {
        return format!(
            "{image} is not in the rootfs registry, so nothing knows which binaries it \
             bakes and this run is NOT gated on its freshness (#667)"
        );
    }
    // Group by (heading, remedy) so a whole-image failure reads as one
    // clause naming several binaries, rather than one clause per binary.
    // Insertion order is preserved: the image side is listed before the
    // target side, which is the order `unverified` builds them in.
    let mut groups: Vec<(&str, &str, Vec<&str>)> = Vec::new();
    for u in unverified {
        let (heading, remedy) = (u.why.heading(), u.why.remedy());
        match groups.iter_mut().find(|(h, r, _)| *h == heading && *r == remedy) {
            Some((_, _, names)) => names.push(&u.binary),
            None => groups.push((heading, remedy, vec![&u.binary])),
        }
    }
    let parts: Vec<String> = groups
        .iter()
        .map(|(heading, remedy, names)| format!("{heading}: {} ({remedy})", names.join(", ")))
        .collect();
    // The two arms differ only in whether anything ELSE was verified, and an
    // operator who reads only the first clause must still learn which.
    let lead = if gated {
        format!(
            "{image} matches this tree in every binary that COULD be checked, but part of it \
             is unverified, so this run is only PARTLY gated on it"
        )
    } else {
        format!("cannot establish whether {image} is current, so this run is NOT gated on it")
    };
    format!("{lead} — {} (#667)", parts.join("; "))
}
