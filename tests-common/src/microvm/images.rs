//! The rootfs-image registry: which images the micro-VM e2e suite boots,
//! which script builds each one, and **which workspace binaries that script
//! bakes into it**.
//!
//! Split out of `microvm.rs` (807 lines) as a movement-only commit — the
//! tree's rule is to split *before* the change that grows a file, in a commit
//! whose `#[test]` name set is verifiable either side — and then extended in
//! the same branch with the baked-binary registry that #667 needs. Against
//! `main` roughly half of this file is new code, so read it as a new module
//! rather than as a move.
//!
//! # Why the baked-binary list is here (issue #667)
//!
//! Every rootfs image is a **copy** of a `target/release/` binary taken at
//! build time — the guest init always, and the worker for all but the
//! browser-driver image, whose worker is Python. So a change to the guest
//! init or to a worker is invisible to the Firecracker e2es until the
//! affected image is rebuilt, and until #667 the suites gave no hint that a
//! rebuild was required. The owed gate for audit item W-2 could have been
//! run start to finish against June images and reported green having tested
//! none of it.
//!
//! Recording *what each image contains, and where* is what lets
//! [`crate::microvm::freshness`] read the baked copy back out and compare it
//! against the code the working tree builds. The list is only as good as its agreement with the
//! scripts, which is why [`tests::the_table_and_the_scripts_agree_on_every_baked_binary`]
//! pins it **in both directions**: a script that starts baking a binary the
//! table does not know about fails the unit test, rather than silently
//! shrinking the staleness reference.

/// One binary copied into an image at build time.
///
/// Two fields because a digest comparison needs both ends: what the working
/// tree builds (`target_name`, under `target/release/`) and where that copy
/// landed inside the image (`in_image`, which is where it must be read back
/// from). The guest init is renamed on the way in — `kastellan-microvm-init`
/// becomes `/sbin/init` — so neither field can be derived from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BakedBinary {
    /// The filename under `target/release/`.
    pub target_name: &'static str,
    /// The absolute path it occupies *inside* the image.
    pub in_image: &'static str,
}

/// A rootfs image the e2e suite boots, and everything needed to tell whether
/// the copy on disk still contains the code the working tree builds.
///
/// An explicit table rather than a derived `build-<stem>-rootfs.sh`
/// convention, because two entries break that convention and a derived name
/// would produce a hint pointing at a file that does not exist:
///
/// * `python-exec.ext4` is built by plain `build-rootfs.sh` (it was the
///   first rootfs, before the per-worker naming settled), and
/// * `kv-demo.ext4`'s script lives under `scripts/workers/kv-demo/`, not
///   `scripts/workers/microvm/` like every other one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootfsImage {
    /// The bare image filename inside [`crate::microvm::image_dir`].
    pub image: &'static str,
    /// The script that builds it, repo-relative.
    pub build_script: &'static str,
    /// The binaries `build_script` copies into the image.
    ///
    /// These are the freshness reference. Python payloads (browser-driver's
    /// driver, python-exec's interpreter) are deliberately **not** listed —
    /// they are not cargo artefacts, so there is no `target/release/` copy
    /// to compare against, and listing them would make the check look
    /// stronger than it is.
    pub baked: &'static [BakedBinary],
}

/// The guest PID 1, baked into **every** image as `/sbin/init`.
///
/// Named once because it is the reference that matters most: it is the only
/// binary all eight images share, so a guest-init change makes every one of
/// them stale at once.
pub const GUEST_INIT_BIN: &str = "kastellan-microvm-init";

/// Where the guest init lands inside every image.
pub const GUEST_INIT_IN_IMAGE: &str = "/sbin/init";

/// The guest init's entry, identical in all eight images.
const INIT: BakedBinary =
    BakedBinary { target_name: GUEST_INIT_BIN, in_image: GUEST_INIT_IN_IMAGE };

/// A worker binary, which every script installs under the same directory.
const fn worker(name: &'static str, in_image: &'static str) -> BakedBinary {
    BakedBinary { target_name: name, in_image }
}

/// Every rootfs image the e2e suite boots. See [`RootfsImage`].
///
/// `every_build_script_exists` pins the scripts against the working tree and
/// `the_table_and_the_scripts_agree_on_every_baked_binary` pins the binary
/// lists AND their destinations, so renaming, moving or re-baking fails a
/// unit test instead of silently misleading whoever hits the failure.
pub const ROOTFS_IMAGES: &[RootfsImage] = &[
    RootfsImage {
        image: "python-exec.ext4",
        build_script: "scripts/workers/microvm/build-rootfs.sh",
        baked: &[
            INIT,
            worker(
                "kastellan-worker-python-exec",
                "/usr/local/bin/kastellan-worker-python-exec",
            ),
        ],
    },
    RootfsImage {
        image: "web-fetch.ext4",
        build_script: "scripts/workers/microvm/build-web-fetch-rootfs.sh",
        baked: &[
            INIT,
            worker("kastellan-worker-web-fetch", "/usr/local/bin/kastellan-worker-web-fetch"),
        ],
    },
    RootfsImage {
        image: "web-search.ext4",
        build_script: "scripts/workers/microvm/build-web-search-rootfs.sh",
        baked: &[
            INIT,
            worker("kastellan-worker-web-search", "/usr/local/bin/kastellan-worker-web-search"),
        ],
    },
    RootfsImage {
        image: "web-research.ext4",
        build_script: "scripts/workers/microvm/build-web-research-rootfs.sh",
        baked: &[
            INIT,
            worker(
                "kastellan-worker-web-research",
                "/usr/local/bin/kastellan-worker-web-research",
            ),
        ],
    },
    RootfsImage {
        image: "browser-driver.ext4",
        build_script: "scripts/workers/microvm/build-browser-driver-rootfs.sh",
        // The driver itself is Python, installed from a docker export — this
        // image bakes no worker binary at all.
        baked: &[INIT],
    },
    RootfsImage {
        image: "matrix.ext4",
        build_script: "scripts/workers/microvm/build-matrix-rootfs.sh",
        baked: &[
            INIT,
            worker("kastellan-worker-matrix", "/usr/local/bin/kastellan-worker-matrix"),
        ],
    },
    RootfsImage {
        image: "net-demo.ext4",
        build_script: "scripts/workers/microvm/build-net-demo-rootfs.sh",
        baked: &[
            INIT,
            worker("kastellan-worker-net-demo", "/usr/local/bin/kastellan-worker-net-demo"),
        ],
    },
    RootfsImage {
        image: "kv-demo.ext4",
        build_script: "scripts/workers/kv-demo/build-kv-demo-rootfs.sh",
        baked: &[
            INIT,
            worker("kastellan-worker-kv-demo", "/usr/local/bin/kastellan-worker-kv-demo"),
        ],
    },
];

/// The shared guest-kernel pin sourced by every `build-*-rootfs.sh`
/// (repo-relative).
///
/// All eight build scripts fetch the *same* `vmlinux`. Before issue #471
/// each one carried its own copy of the URL, the arch `case`, and an
/// unchecked `curl`. This file is now the single place any of that is
/// written down; `kernel_pin_is_the_only_place_the_kernel_url_appears`
/// keeps it that way.
pub const GUEST_KERNEL_LIB: &str = "scripts/workers/microvm/lib/guest-kernel.sh";

/// The one script that produces every `target/release/` binary an image bakes
/// (repo-relative), and the only way a `build-*-rootfs.sh` may cause cargo to
/// run at all (issue #682).
///
/// # Why a shared producer rather than eight `cargo build -p …` lines
///
/// Cargo's feature unification is per *invocation*: the features other
/// selected packages enable on a shared dependency are unified into that
/// dependency's build, so **the package selection changes the bytes of an
/// otherwise identical binary**. Measured on the DGX at `ec9a2e94`, back and
/// forth, deterministic each way — `8a21877a…` is the reference the images
/// bake since this branch, `669821d3…` is what they baked before it:
///
/// ```text
/// cargo build --release -p kastellan-microvm-init   669821d3…  (what images USED to bake)
/// cargo build --release --workspace                 8a21877a…  (the reference now)
/// ```
///
/// Each `build-*-rootfs.sh` used a narrow `-p` set while the deploy path
/// (`scripts/upgrade_from_git.sh`) runs this script, so after any deploy the
/// freshness check compared an image against a binary **no image is ever
/// built from** and declared all eight stale. Every image was correct. That
/// is fail-closed, so it was noise rather than a containment hole — but it is
/// exactly the *"a check that cries wolf on the common case is a check
/// somebody switches off"* failure [`super::freshness`] was designed to
/// avoid, and it left the whole Firecracker tier unrunnable after a normal
/// build unless you knew to re-run each image's exact `-p` line.
///
/// Pointing every producer at one script removes the ambiguity at its source
/// instead of teaching the gate to tolerate it: the bytes in the image, the
/// bytes the deploy ships and the bytes the gate reads all come from the same
/// script. It also picks up the `live-matrix` step — `--workspace` alone
/// builds `kastellan-worker-matrix` *without* its feature and that worker
/// refuses to run, which is why this script exists at all.
///
/// # ⚠️ One script, two invocations, last writer wins
///
/// This is **not** "one build by construction", and overstating it is how the
/// next person gets caught. The script runs `--workspace` and then a narrow
/// `-p kastellan-worker-matrix --features live-matrix`; both write
/// `target/release/kastellan-worker-matrix` and the second wins, which is why
/// [`tests::the_canonical_producer_is_the_one_the_deploy_path_runs`] pins
/// that **order** as well as the contents.
///
/// The consequence is that the result is only stable until something else
/// touches that path. Measured on the Mac, 2026-09-08: after the live-matrix
/// build (`0ca537c2…`), a bare `cargo build --release --workspace` re-uplifted
/// the non-featured artefact (`9d29cd49…`) in **0.32 s** with no compilation
/// and no output but `Finished`. That state ships a Matrix worker which
/// refuses to run, and makes `matrix.ext4` read stale here. So the rule is
/// *run this script last*, and the script prints the matrix digest to make
/// the flip visible rather than something a stale-image panic reveals later.
///
/// # Cost
///
/// Warm, it is free: cargo keeps both artefact sets under different
/// `-C metadata` hashes, so flipping selections re-links from cache — both
/// measured builds above finished in **3.00 s** on a warm tree, and
/// `rebuild-all-rootfs.sh`'s eight invocations cost about five seconds
/// between them.
///
/// Cold, it is not free and the docs should not pretend otherwise. Building
/// one image on a fresh checkout went from a two-crate closure
/// (`-p kastellan-microvm-init`) to the whole workspace (392 crates) plus the
/// `matrix-rust-sdk` subtree — measured at 4 m 09 s for the live-matrix step
/// alone — even for `kv-demo`, `net-demo` and `browser-driver`, which have
/// nothing to do with Matrix. That is the price of removing the ambiguity at
/// the producer, and it is paid once per checkout rather than per image.
///
/// # What is out of scope, deliberately
///
/// `scripts/workers/python-exec/build-image.sh` also runs `cargo build
/// --release`, and correctly so: it cross-builds inside an Apple `container`
/// with its own `--target-dir`, never writes this tree's `target/release/`,
/// and so cannot take part in the skew. The rule here is about who writes
/// `target/release/`, not about the string `cargo build`.
///
/// [`tests::no_rootfs_build_script_runs_its_own_cargo_build`] is what keeps a
/// ninth script from growing its own copy again — the same drift channel
/// [`GUEST_KERNEL_LIB`] closed for eight unchecked `curl`s (#471) — and
/// [`tests::every_rootfs_build_script_on_disk_is_registered`] is what stops a
/// ninth script from simply never being registered, which would put it
/// outside every scanner in this module.
pub const RELEASE_BUILD_SCRIPT: &str = "scripts/build-release.sh";

/// The one script that rebuilds every image in [`ROOTFS_IMAGES`].
///
/// Exists because the build scripts live in **two** directories, which makes
/// "rebuild everything" easy to get wrong — the #667 session that filed this
/// rebuilt them by hand-listing paths twice. Every operator-facing staleness
/// message names this rather than making the reader assemble the list.
pub const REBUILD_ALL_SCRIPT: &str = "scripts/workers/microvm/rebuild-all-rootfs.sh";

/// The registry entry for `rootfs`, or `None` for an image this table does
/// not know about.
///
/// Pure — no filesystem access, so it is unit-testable on any host.
pub fn image_entry(rootfs: &str) -> Option<&'static RootfsImage> {
    ROOTFS_IMAGES.iter().find(|e| e.image == rootfs)
}

/// The build script for `rootfs`, or `None` for an image this table does
/// not know about.
///
/// Pure — no filesystem access, so it is unit-testable on any host.
/// Callers fold the `None` case into a generic hint rather than guessing
/// a filename; a guessed hint is the failure mode this module exists to
/// prevent.
pub fn build_script_for(rootfs: &str) -> Option<&'static str> {
    image_entry(rootfs).map(|e| e.build_script)
}

/// The binaries baked into `rootfs`, or an empty slice for an image this
/// table does not know about.
///
/// Empty is the honest answer for an unknown image, and it is *load-bearing*:
/// [`crate::microvm::freshness`] turns "nothing to compare" into
/// `Indeterminate` rather than into a silent pass, so an image the table has
/// never heard of cannot be reported fresh.
pub fn baked_for(rootfs: &str) -> &'static [BakedBinary] {
    image_entry(rootfs).map(|e| e.baked).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::microvm::repo_root;
    use crate::microvm::script_scan::{cargo_invocations, code_body, invokes, sources_of};

    /// The table must not be empty, or every `for entry in ROOTFS_IMAGES`
    /// below passes having checked nothing.
    ///
    /// Non-emptiness *is* pinned from `rebuild_script_tests` and
    /// `preflight_tests`, but both of those are about a different script and
    /// would leave with it. A loop guard belongs next to the loops.
    #[test]
    fn the_registry_is_not_empty() {
        assert!(
            ROOTFS_IMAGES.len() >= 8,
            "every scanner in this module loops over ROOTFS_IMAGES and would pass \
             vacuously if it shrank; got {}",
            ROOTFS_IMAGES.len()
        );
    }

    /// Every hint must name a script that actually exists, so a rename
    /// or a move breaks this test rather than silently sending an
    /// operator to a nonexistent path. This is the pin that lets the
    /// table stay hand-written instead of derived.
    #[test]
    fn every_build_script_exists() {
        let root = repo_root();
        for entry in ROOTFS_IMAGES {
            let path = root.join(entry.build_script);
            assert!(
                path.is_file(),
                "build script for {} is missing: {}",
                entry.image,
                path.display()
            );
        }
    }

    #[test]
    fn build_script_lookup_hits_the_two_convention_breakers() {
        // Neither of these follows `build-<stem>-rootfs.sh` under
        // `scripts/workers/microvm/`, which is why the table is explicit.
        assert_eq!(
            build_script_for("python-exec.ext4"),
            Some("scripts/workers/microvm/build-rootfs.sh")
        );
        assert_eq!(
            build_script_for("kv-demo.ext4"),
            Some("scripts/workers/kv-demo/build-kv-demo-rootfs.sh")
        );
    }

    #[test]
    fn build_script_is_none_for_an_unknown_rootfs() {
        // Callers must fall back to a generic hint, never guess a name.
        assert_eq!(build_script_for("not-a-real-worker.ext4"), None);
    }

    /// #667's structural pin, and the one that keeps the freshness check
    /// honest: the table's binary list must equal what the script actually
    /// copies out of `target/release/`, **in both directions**.
    ///
    /// The `⊆` half alone would be satisfied by an empty list, and an empty
    /// list yields `Indeterminate` — a check that silently stops checking.
    /// The `⊇` half is the one that matters in practice: a script that grows
    /// a new baked binary must fail here rather than quietly narrow the
    /// freshness reference to the binaries somebody remembered.
    #[test]
    fn the_table_and_the_scripts_agree_on_every_baked_binary() {
        let root = repo_root();
        for entry in ROOTFS_IMAGES {
            let raw = std::fs::read_to_string(root.join(entry.build_script))
                .unwrap_or_else(|e| panic!("read {}: {e}", entry.build_script));
            let body = code_body(&raw);

            // Every `target/release/<name>` the script mentions, deduped.
            let mut in_script: Vec<&str> = body
                .match_indices("target/release/")
                .map(|(i, m)| {
                    let rest = &body[i + m.len()..];
                    let end = rest
                        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                        .unwrap_or(rest.len());
                    &rest[..end]
                })
                .collect();
            // A capture that stops immediately is `target/release/$VAR` or
            // `${BIN}` — the scanner cannot see through the interpolation.
            // Dropping it silently would narrow the freshness reference to
            // the binaries somebody remembered, which is #667 with extra
            // steps; refuse instead.
            assert!(
                in_script.iter().all(|n| !n.is_empty()),
                "{} interpolates a variable into target/release/, so this scanner \
                 cannot tell what it bakes — spell the binary name literally",
                entry.build_script
            );
            in_script.sort_unstable();
            in_script.dedup();

            let mut in_table: Vec<&str> = entry.baked.iter().map(|b| b.target_name).collect();
            in_table.sort_unstable();
            in_table.dedup();

            assert_eq!(
                in_table, in_script,
                "{} bakes {in_script:?} but the table for {} says {in_table:?} — \
                 the freshness check is only as good as this agreement",
                entry.build_script, entry.image
            );
        }
    }

    /// Every rootfs build script must get its `target/release/` binaries from
    /// the one canonical producer, and no build it drives may run cargo
    /// itself (issue #682).
    ///
    /// # Why both halves are needed
    ///
    /// The presence half alone is satisfied by a script that calls the
    /// producer *and* keeps its old narrow `cargo build -p …` line. Whichever
    /// of the two runs last then wins, so the image's bytes become
    /// order-dependent — which is the ambiguity #682 is about, not merely a
    /// slower build. The absence half alone is satisfied by a script that
    /// builds nothing at all and bakes whatever happens to be lying in
    /// `target/release/`, which is #667 restored.
    ///
    /// # Why it follows the `source`
    ///
    /// The scan covers the script **and every in-repo file it sources**
    /// ([`sources_of`]). All eight already source `lib/guest-kernel.sh`, so a
    /// scan of the script text alone would be evaded by moving one `cargo
    /// build` into the shared lib — the drift channel reopened one
    /// indirection out. The detection itself is
    /// [`cargo_invocations`], a pure function over the text, so
    /// `the_cargo_scan_reports_a_planted_invocation` can prove it detects.
    #[test]
    fn no_rootfs_build_script_runs_its_own_cargo_build() {
        let root = repo_root();
        for entry in ROOTFS_IMAGES {
            let scanned = sources_of(&root, entry.build_script);
            let (_, script_text) = &scanned[0];

            assert!(
                invokes(script_text, RELEASE_BUILD_SCRIPT),
                "{} must build its binaries with `bash {RELEASE_BUILD_SCRIPT}` — the \
                 one producer the deploy path also uses. Without it the image is \
                 baked from a package selection nothing else shares, and the \
                 freshness check compares against bytes no image was built from (#682)",
                entry.build_script
            );

            for (path, text) in &scanned {
                let own = cargo_invocations(text);
                assert!(
                    own.is_empty(),
                    "{path} (reached from {}) runs cargo itself ({own:?}) — package \
                     selection changes the BYTES of an identical binary, so a second \
                     invocation makes the image's bytes depend on which ran last and \
                     reinstates #682. Let {RELEASE_BUILD_SCRIPT} be the only one",
                    entry.build_script
                );
            }
        }
    }

    /// The positive control for the test above.
    ///
    /// Plants each violation into a copy of a **real** build script's text
    /// and requires the scan to come back with it. Without this,
    /// `assert!(own.is_empty())` is green whether the loop found nothing or
    /// the detector cannot detect — the failure
    /// [`crate::microvm::call_site_tests`] documents and the reason its scan
    /// is a pure function too.
    #[test]
    fn the_cargo_scan_reports_a_planted_invocation() {
        let root = repo_root();
        let script = ROOTFS_IMAGES[0].build_script;
        let clean = std::fs::read_to_string(root.join(script))
            .unwrap_or_else(|e| panic!("read {script}: {e}"));
        assert!(cargo_invocations(&clean).is_empty(), "{script} must start clean");
        assert!(invokes(&clean, RELEASE_BUILD_SCRIPT), "{script} must call the producer");

        // Each of these evaded the literal `contains("cargo build")` this
        // scan replaced; the last is the shape that hid behind a quoted `#`.
        for planted in [
            "cargo build --release -p kastellan-microvm-init",
            "cargo  build --release -p kastellan-microvm-init",
            "\"$CARGO\" build --release -p kastellan-microvm-init",
            "cargo rustc --release -p kastellan-microvm-init",
            "echo \"step # 2\" && cargo build --release -p kastellan-microvm-init",
        ] {
            let mutant = format!("{clean}{planted}\n");
            let found = cargo_invocations(&mutant);
            assert_eq!(found.len(), 1, "planted `{planted}` must be caught, got {found:?}");
        }

        // ...and the presence half must fail when the producer goes away,
        // or a script that builds nothing at all would pass.
        let without = clean.replace(RELEASE_BUILD_SCRIPT, "scripts/nothing.sh");
        assert!(
            !invokes(&without, RELEASE_BUILD_SCRIPT),
            "dropping the producer must be visible, or the presence half is vacuous"
        );
    }

    /// A ninth build script that never lands in [`ROOTFS_IMAGES`] is invisible
    /// to every scanner in this module, which all loop over the registry.
    ///
    /// So the registry is pinned against the filesystem, not just the
    /// filesystem against the registry (`every_build_script_exists` does
    /// that). Without this, "a ninth script cannot grow its own cargo build"
    /// is only true of a ninth script somebody remembered to register.
    #[test]
    fn every_rootfs_build_script_on_disk_is_registered() {
        let root = repo_root();
        let registered: Vec<&str> = ROOTFS_IMAGES.iter().map(|e| e.build_script).collect();
        for dir in ["scripts/workers/microvm", "scripts/workers/kv-demo"] {
            let entries = std::fs::read_dir(root.join(dir))
                .unwrap_or_else(|e| panic!("read dir {dir}: {e}"));
            for entry in entries {
                let name = entry.expect("dir entry").file_name();
                let name = name.to_string_lossy().to_string();
                if !(name.starts_with("build-") && name.ends_with("rootfs.sh")) {
                    continue;
                }
                let rel = format!("{dir}/{name}");
                assert!(
                    registered.contains(&rel.as_str()),
                    "{rel} builds a rootfs but is not in ROOTFS_IMAGES, so no freshness \
                     check knows what it bakes and no guard sees what it runs (#667/#682)"
                );
            }
        }
    }

    /// The prologue that makes a build script cwd-independent (#686).
    ///
    /// ⚠️ **One spelling, compared byte-for-byte against all eight scripts.**
    /// Seven of the eight used bare `target/release/...` paths and failed
    /// partway through with `install: cannot stat` when run from anywhere but
    /// the workspace root; the eighth had solved it a third way. #669's lesson
    /// was to *count the producers* — a rule written out in eight places is
    /// eight chances to drift, and two of the three hand-spelled copies of the
    /// bwrap userns pair were wrong.
    ///
    /// Because every script is asserted to contain this exact text,
    /// [`tests::the_prologue_lands_on_the_workspace_root_from_a_foreign_cwd`]
    /// can execute **this** string and thereby cover all eight behaviourally,
    /// rather than proving eight times over what a script merely *says*.
    const REPO_ROOT_PROLOGUE: &str = r#"REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)" || REPO_ROOT=""
if [ -z "$REPO_ROOT" ] || ! cd "$REPO_ROOT"; then
    echo "Cannot locate the workspace root from ${BASH_SOURCE[0]}" >&2; exit 1
fi"#;

    /// Every rootfs build script anchors itself on the workspace root.
    ///
    /// A hand-run of a single script from a subdirectory used to die at
    /// `install: cannot stat 'target/release/…'`, which names a missing file
    /// rather than the wrong cwd — a diagnostic pointing at the wrong cause,
    /// which is the family of defect this whole module keeps finding.
    #[test]
    fn every_build_script_anchors_on_the_workspace_root() {
        let root = repo_root();
        let mut missing: Vec<&str> = Vec::new();
        for entry in ROOTFS_IMAGES {
            let src = std::fs::read_to_string(root.join(entry.build_script))
                .unwrap_or_else(|e| panic!("read {}: {e}", entry.build_script));
            if !src.contains(REPO_ROOT_PROLOGUE) {
                missing.push(entry.build_script);
            }
        }
        assert!(
            missing.is_empty(),
            "these run with the operator's cwd and fail at `install: cannot stat \
             target/release/…` when invoked from anywhere but the workspace root (#686). \
             Add the prologue verbatim, after the `source` of `lib/guest-kernel.sh`:\n{}",
            missing.join("\n")
        );
    }

    /// ⚠️ **Behavioural, not textual: the prologue is RUN, from a directory it
    /// was not invoked from, and asserted to arrive at the workspace root.**
    ///
    /// The eight files had no behavioural coverage at all before this — every
    /// existing check reads what they *say*. A prologue that parses, contains
    /// the right words and lands in the wrong directory would have satisfied
    /// all of them.
    #[test]
    fn the_prologue_lands_on_the_workspace_root_from_a_foreign_cwd() {
        // A stand-in workspace root, three levels above the script — the same
        // depth as `scripts/workers/<group>/`, which is what `../../..` means.
        let tmp = std::env::temp_dir().join(format!("kastellan-686-{}", std::process::id()));
        let script_dir = tmp.join("scripts/workers/microvm");
        std::fs::create_dir_all(&script_dir).expect("create the stand-in tree");
        let script = script_dir.join("probe.sh");
        std::fs::write(&script, format!("set -euo pipefail\n{REPO_ROOT_PROLOGUE}\npwd\n"))
            .expect("write the probe script");

        let mut cmd = std::process::Command::new("bash");
        // Invoked with a RELATIVE path from a foreign cwd, which is the case a
        // prologue placed before the `source` line would get wrong.
        cmd.arg("scripts/workers/microvm/probe.sh").current_dir(&tmp);
        let out = kastellan_sandbox::bounded_command::probe_output(
            &mut cmd,
            kastellan_sandbox::bounded_command::PROBE_BUDGET,
        )
        .unwrap_or_else(|e| panic!("the probe script did not answer: {e:?}"));

        let landed = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let expected = std::fs::canonicalize(&tmp).expect("canonicalize the stand-in root");
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(out.status.success(), "the prologue exited non-zero: {}", landed);
        assert_eq!(
            std::path::Path::new(&landed),
            expected,
            "the prologue landed in {landed}, not the workspace root"
        );
    }

    /// ⚠️ **A root that cannot be resolved STOPS the script — it does not fall
    /// through to the operator's cwd.**
    ///
    /// Behavioural, and it drives the real prologue text down its failure arm
    /// by making `dirname` hand back a path nothing can `cd` into. That arm is
    /// the reason the prologue is four lines rather than one, and until now
    /// nothing reached it.
    ///
    /// ⚠️ **The premise behind it is bash-version-dependent, and the two dev
    /// hosts disagree today — measured 2026-09-10, not assumed:**
    ///
    /// | bash | `cd ""` |
    /// | --- | --- |
    /// | 3.2.57 (macOS `/bin/bash`) | exit **0** |
    /// | 5.2.21 (DGX) | exit **0** |
    /// | 5.3.15 (Homebrew, dev Mac) | exit **1**, `cd: null directory` |
    ///
    /// So `cd ""` silently succeeding is real on two of the three bashes this
    /// project runs on, and the `[ -z "$REPO_ROOT" ]` half of the guard is
    /// load-bearing — while a test asserting bash's behaviour directly would
    /// pass on one host and fail on the other. This asserts the **guard's**
    /// behaviour instead, which is the same on every bash.
    #[test]
    fn an_unresolvable_root_stops_the_script_rather_than_using_the_cwd() {
        let tmp = std::env::temp_dir().join(format!("kastellan-686-neg-{}", std::process::id()));
        let script_dir = tmp.join("scripts/workers/microvm");
        let stub_bin = tmp.join("stub");
        std::fs::create_dir_all(&script_dir).expect("create the stand-in tree");
        std::fs::create_dir_all(&stub_bin).expect("create the stub bin");
        std::fs::write(
            script_dir.join("probe.sh"),
            format!("set -euo pipefail\n{REPO_ROOT_PROLOGUE}\npwd\n"),
        )
        .expect("write the probe script");
        // `cd` and `pwd` are shell builtins, so `dirname` is the only external
        // the prologue depends on — shadowing it is enough to drive the
        // command substitution to failure without editing the prologue.
        let stub = stub_bin.join("dirname");
        std::fs::write(&stub, "#!/bin/sh\necho /kastellan-686-no-such-directory\n")
            .expect("write the stub dirname");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod the stub");
        }

        let path = format!(
            "{}:{}",
            stub_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("scripts/workers/microvm/probe.sh").current_dir(&tmp).env("PATH", path);
        let out = kastellan_sandbox::bounded_command::probe_output(
            &mut cmd,
            kastellan_sandbox::bounded_command::PROBE_BUDGET,
        )
        .unwrap_or_else(|e| panic!("the probe script did not answer: {e:?}"));

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            !out.status.success(),
            "an unresolvable root must stop the script; it printed {stdout:?} and exited 0"
        );
        assert!(
            stderr.contains("Cannot locate the workspace root"),
            "the failure must name its own cause, not leave the operator guessing: {stderr:?}"
        );
        assert!(
            stdout.is_empty(),
            "the script continued past the guard and reached `pwd`: {stdout:?}"
        );
    }

    /// Every build script must at least PARSE, which no amount of text
    /// scanning can tell you.
    ///
    /// This module reads these files as text and asserts what they say. That
    /// leaves a whole class untouched: an edit that satisfies every scanner
    /// and does not run. `bash -n` is the cheapest possible check that the
    /// files are still shell, and it works on both hosts.
    #[test]
    fn every_build_script_parses() {
        let root = repo_root();
        for entry in ROOTFS_IMAGES {
            // Bounded (#690), for the same reason as every other shell-out in
            // this module: `bash -n` on a local file has no business taking
            // ten seconds, and an unbounded one that did would stall the sweep
            // with nothing to read.
            let mut cmd = std::process::Command::new("bash");
            cmd.arg("-n").arg(root.join(entry.build_script));
            let out = kastellan_sandbox::bounded_command::probe_output(
                &mut cmd,
                kastellan_sandbox::bounded_command::PROBE_BUDGET,
            )
            .unwrap_or_else(|e| panic!("`bash -n {}` did not answer: {e:?}", entry.build_script));
            assert!(
                out.status.success(),
                "{} is not valid bash: {}",
                entry.build_script,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The canonical producer must exist, must be what the deploy path runs,
    /// and must still be a **workspace** build — otherwise the whole argument
    /// collapses with every other guard green.
    ///
    /// The existence half alone is worthless: a `build-release.sh` that
    /// nothing deploys with would make the images agree with a build nobody
    /// else performs, which is #682 with the odd one out swapped rather than
    /// removed. And the deploy half alone would still pass if line 1 of the
    /// producer were edited to a narrow `-p` set, which is #682 itself moved
    /// one file over.
    ///
    /// ⚠️ The `-p` allowance is deliberate and narrow. The producer ends with
    /// `-p kastellan-worker-matrix --features live-matrix` **on purpose** —
    /// `--workspace` builds that worker without its feature and it then
    /// refuses to run. Both invocations write
    /// `target/release/kastellan-worker-matrix` and the last one wins, so the
    /// order is load-bearing too: the workspace build must come FIRST.
    #[test]
    fn the_canonical_producer_is_the_one_the_deploy_path_runs() {
        let root = repo_root();
        assert!(
            root.join(RELEASE_BUILD_SCRIPT).is_file(),
            "{RELEASE_BUILD_SCRIPT} is missing, so every build script now names \
             a path that does not exist"
        );
        let deploy = "scripts/upgrade_from_git.sh";
        let deploy_body = std::fs::read_to_string(root.join(deploy))
            .unwrap_or_else(|e| panic!("read {deploy}: {e}"));
        assert!(
            invokes(&deploy_body, RELEASE_BUILD_SCRIPT),
            "{deploy} no longer builds with {RELEASE_BUILD_SCRIPT}, so the bytes \
             a deploy installs and the bytes an image bakes have diverged again \
             (#682) — point both at the same producer"
        );

        let producer = std::fs::read_to_string(root.join(RELEASE_BUILD_SCRIPT))
            .unwrap_or_else(|e| panic!("read {RELEASE_BUILD_SCRIPT}: {e}"));
        let builds = cargo_invocations(&producer);
        let workspace = builds.iter().position(|l| l.contains("--workspace"));
        assert_eq!(
            workspace,
            Some(0),
            "{RELEASE_BUILD_SCRIPT} must build the WHOLE workspace, and first — a \
             narrow selection here would silently make every image agree with a \
             build nothing else performs (#682), and a later --workspace would \
             overwrite the live-matrix worker. Got {builds:?}"
        );
        for extra in &builds[1..] {
            assert!(
                extra.contains("kastellan-worker-matrix") && extra.contains("live-matrix"),
                "{RELEASE_BUILD_SCRIPT} runs a second selection that is not the \
                 sanctioned live-matrix rebuild: {extra}. Every extra invocation \
                 overwrites what --workspace just wrote"
            );
        }
    }

    /// The destination matters as much as the name: the digest is read back
    /// from `in_image`, so a wrong path yields `Indeterminate` — a check that
    /// silently stops checking, which is #667 with extra steps. Every script
    /// installs into `"$WORK<in_image>"`, so that literal must appear.
    #[test]
    fn the_table_and_the_scripts_agree_on_every_in_image_destination() {
        let root = repo_root();
        for entry in ROOTFS_IMAGES {
            let raw = std::fs::read_to_string(root.join(entry.build_script))
                .unwrap_or_else(|e| panic!("read {}: {e}", entry.build_script));
            let body = code_body(&raw);
            for b in entry.baked {
                let dest = format!("\"$WORK{}\"", b.in_image);
                assert!(
                    body.contains(&dest),
                    "{} never installs {} to {} — the freshness check would read \
                     the wrong path and report Indeterminate forever",
                    entry.build_script,
                    b.target_name,
                    b.in_image
                );
            }
        }
    }

    /// The guest init is in every image, so a guest-init change makes
    /// *every* image stale. If an entry ever lost it the freshness check
    /// would go blind for that image alone, which is the hardest kind of
    /// gap to notice.
    #[test]
    fn every_image_bakes_the_guest_init() {
        for entry in ROOTFS_IMAGES {
            assert!(
                entry.baked.iter().any(|b| b.target_name == GUEST_INIT_BIN),
                "{} does not list {GUEST_INIT_BIN}; every image bakes the guest PID 1",
                entry.image
            );
        }
    }

    /// The init is renamed on the way in, so `in_image` can never be derived
    /// from `target_name`. Pinned because a future "simplification" that
    /// derived it would silently break every image's strongest reference.
    #[test]
    fn the_guest_init_is_renamed_to_sbin_init_inside_every_image() {
        for entry in ROOTFS_IMAGES {
            let init = entry
                .baked
                .iter()
                .find(|b| b.target_name == GUEST_INIT_BIN)
                .unwrap_or_else(|| panic!("{} has no guest init", entry.image));
            assert_eq!(init.in_image, GUEST_INIT_IN_IMAGE, "{}", entry.image);
        }
    }

    #[test]
    fn baked_binaries_are_empty_for_an_unknown_rootfs() {
        // Load-bearing: empty means Indeterminate downstream, never a
        // silent pass for an image nothing knows how to check.
        assert!(baked_for("mystery.ext4").is_empty());
    }
}
