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
/// (repo-relative), and the *only* cargo invocation a `build-*-rootfs.sh` may
/// use (issue #682).
///
/// # Why a shared producer rather than eight `cargo build -p …` lines
///
/// Cargo's feature unification is per *invocation*: the features other
/// selected packages enable on a shared dependency are unified into that
/// dependency's build, so **the package selection changes the bytes of an
/// otherwise identical binary**. Measured on the DGX at `ec9a2e94`, back and
/// forth, deterministic each way:
///
/// ```text
/// cargo build --release -p kastellan-microvm-init   669821d3…  (== every image's baked copy)
/// cargo build --release --workspace                 8a21877a…
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
/// bytes the deploy ships and the bytes the gate reads are then the same
/// build by construction. It also picks up the `live-matrix` step for free —
/// `--workspace` alone builds `kastellan-worker-matrix` *without* its feature
/// and that worker refuses to run, which is why this script exists at all.
///
/// The cost is small enough to be a non-issue: cargo keeps both artefact sets
/// under different `-C metadata` hashes, so flipping selections re-links from
/// cache. Both measured builds above finished in **3.00 s** on a warm tree.
///
/// [`tests::no_rootfs_build_script_runs_its_own_cargo_build`] is what keeps a
/// ninth script from growing its own copy again — the same drift channel
/// [`GUEST_KERNEL_LIB`] closed for eight unchecked `curl`s (#471).
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

    /// The executable part of a shell line: everything before its trailing comment.
    ///
    /// Every scanner over a `build-*-rootfs.sh` must ignore prose, or a comment
    /// that merely *names* a path or a command reads as one being run. That is not
    /// hypothetical: the #682 note added to all eight build scripts contains the
    /// words `target/release/` and `cargo build -p`, and tripped **two** of the
    /// scanners below within minutes of being written — one of them by claiming
    /// the script interpolates a variable it does not have.
    ///
    /// A `#` only opens a comment when it begins a word, so `${VAR#prefix}` and
    /// `"a#b"` survive. The error direction is deliberate: a `#` this rule misses
    /// leaves *more* text to scan, never less, so it cannot quietly weaken a
    /// fail-closed guard — it can only make one complain.
    fn code_of(line: &str) -> &str {
        let bytes = line.as_bytes();
        for (i, &c) in bytes.iter().enumerate() {
            if c == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
                return &line[..i];
            }
        }
        line
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
            let body: String =
                raw.lines().map(code_of).collect::<Vec<_>>().join("\n");

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

    /// The comment rule is load-bearing for three scanners, so it is tested
    /// directly rather than only through them.
    ///
    /// The `${VAR#prefix}` case is the one that would matter if a build script
    /// ever grew parameter expansion on an `install` line: stripping there
    /// would hide a real bake and silently narrow the freshness reference,
    /// which is #667 with extra steps.
    #[test]
    fn code_of_strips_prose_and_leaves_shell_syntax_alone() {
        assert_eq!(code_of("install -D target/release/foo \"$WORK/bin/foo\""),
                   "install -D target/release/foo \"$WORK/bin/foo\"");
        assert_eq!(code_of("# target/release/phantom is only mentioned"), "");
        assert_eq!(code_of("mkdir -p \"$WORK/run\"   # slice 4a"), "mkdir -p \"$WORK/run\"   ");
        // A `#` that does not begin a word is shell syntax, not a comment.
        assert_eq!(code_of("script_of() { echo \"${1#*:}\"; }"), "script_of() { echo \"${1#*:}\"; }");
        assert_eq!(code_of("no comment here"), "no comment here");
    }

    /// Every rootfs build script must get its `target/release/` binaries from
    /// the one canonical producer, and must not run cargo itself (issue #682).
    ///
    /// # Why both halves are needed
    ///
    /// The `contains` half alone is satisfied by a script that calls the
    /// producer *and* keeps its old narrow `cargo build -p …` line — and
    /// since that line would run second, it would overwrite the canonical
    /// bytes with the very ones #682 is about. The `!contains` half alone is
    /// satisfied by a script that builds nothing at all and bakes whatever
    /// happens to be lying in `target/release/`, which is #667 restored.
    ///
    /// This is the pin that lets the convention stay a convention: a ninth
    /// script that grows its own `cargo build` fails here rather than
    /// silently reintroducing the skew. Same drift channel, same remedy as
    /// [`GUEST_KERNEL_LIB`] (#471).
    #[test]
    fn no_rootfs_build_script_runs_its_own_cargo_build() {
        let root = repo_root();
        for entry in ROOTFS_IMAGES {
            let raw = std::fs::read_to_string(root.join(entry.build_script))
                .unwrap_or_else(|e| panic!("read {}: {e}", entry.build_script));
            let code: Vec<&str> = raw.lines().map(code_of).collect();

            let invocation = format!("bash {RELEASE_BUILD_SCRIPT}");
            assert!(
                code.iter().any(|l| l.contains(&invocation)),
                "{} must build its binaries with `{invocation}` — the one producer \
                 the deploy path also uses. Without it the image is baked from a \
                 package selection nothing else shares, and the freshness check \
                 compares against bytes no image was built from (#682)",
                entry.build_script
            );

            // Prose is excluded via the shared rule: the scripts explain WHY
            // they no longer run cargo, and naming the command they replaced
            // must not read as running it.
            let own_cargo: Vec<&&str> =
                code.iter().filter(|l| l.contains("cargo build")).collect();
            assert!(
                own_cargo.is_empty(),
                "{} runs its own cargo build ({own_cargo:?}) — package selection \
                 changes the BYTES of an identical binary, so a second invocation \
                 overwrites the canonical reference and reinstates #682. Let \
                 {RELEASE_BUILD_SCRIPT} be the only one",
                entry.build_script
            );
        }
    }

    /// The canonical producer must exist, and must itself be what the deploy
    /// path runs — otherwise the whole argument collapses.
    ///
    /// Without the second half this is a file-existence check: a
    /// `build-release.sh` that nothing deploys with would make the images
    /// agree with a build **nobody else performs**, which is #682 with the
    /// odd one out swapped rather than removed.
    #[test]
    fn the_canonical_producer_is_the_one_the_deploy_path_runs() {
        let root = repo_root();
        assert!(
            root.join(RELEASE_BUILD_SCRIPT).is_file(),
            "{RELEASE_BUILD_SCRIPT} is missing, so every build script now names \
             a path that does not exist"
        );
        let deploy = "scripts/upgrade_from_git.sh";
        let body = std::fs::read_to_string(root.join(deploy))
            .unwrap_or_else(|e| panic!("read {deploy}: {e}"));
        assert!(
            body.contains(&format!("bash {RELEASE_BUILD_SCRIPT}")),
            "{deploy} no longer builds with {RELEASE_BUILD_SCRIPT}, so the bytes \
             a deploy installs and the bytes an image bakes have diverged again \
             (#682) — point both at the same producer"
        );
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
            let body: String =
                raw.lines().map(code_of).collect::<Vec<_>>().join("\n");
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
