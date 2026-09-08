//! Is the macOS Apple-`container` micro-VM tier usable, and does its image
//! contain the code under test? (issues #684, #687)
//!
//! # The two defects this closes
//!
//! #667/#679/#683 gave the **Firecracker** tier a freshness gate and a
//! REQUIRE knob. `CLAUDE.md` makes cross-platform parity a hard constraint,
//! and the macOS Apple-`container` tier had neither. Three suites
//! (`python_exec_container_e2e`, `python_exec_warm_idle_e2e`,
//! `lifecycle_container_routing_e2e`) each carried their own byte-copied
//! precondition helper — the same drift #679 retired on the Linux side, where
//! four private copies of `egress_proxy_bin_or_skip` had to be deleted one by
//! one.
//!
//! Each copy folded two unrelated host facts into one verdict:
//!
//! ```ignore
//! let listed = Command::new("container").args(["image", "list"]).output();
//! let has_image = matches!(
//!     listed,
//!     Ok(o) if String::from_utf8_lossy(&o.stdout).contains("python-exec")
//! );
//! ```
//!
//! * An `Err` from the spawn — CLI absent, system service down — became
//!   "image not present", sending the operator to `build-image.sh` for a
//!   problem no build can fix. [`image_or_reason`] separates the two.
//! * `contains("python-exec")` matches **any** tag containing that substring,
//!   so a stale or hand-tagged image certified the run. [`find_image`] matches
//!   a normalised, *exact* reference.
//!
//! # Why the reference is a TIMESTAMP here and a DIGEST there
//!
//! #667 compares the sha256 of the binary baked into each `.ext4` against
//! `target/release/<binary>`. That rule cannot be carried across, and the
//! reason is structural rather than a matter of effort:
//! `scripts/workers/python-exec/build-image.sh` **cross-builds** the worker
//! for the guest (linux/arm64) inside a `rust:1-slim-bookworm` container with
//! its own `--target-dir`, then copies the result into the runtime image. The
//! host `target/release/kastellan-worker-python-exec` is a macOS Mach-O
//! binary. The two are different architectures and different object formats;
//! they will never compare equal, and nothing on the host can reproduce the
//! guest bytes without re-running the cross-build.
//!
//! ⚠️ Recording the baked digest at build time does not rescue it either —
//! that is the sidecar-manifest option #682 measured and rejected, because a
//! recorded digest is by construction equal to what is in the image, so the
//! comparison degenerates to "the file I wrote says what I wrote".
//! Establishing *"was this image built from this source?"* needs a
//! **source-side** reference.
//!
//! # Why mtime works here and did not work for #667
//!
//! #667's module docs reject mtime, so re-using it needs its own
//! justification rather than an appeal to symmetry. The two rules compare
//! different things:
//!
//! * #667 compared an image against **`target/release/<binary>`** — a *build
//!   output*. Cargo relinks byte-identical binaries and moves their mtime, so
//!   six correct DGX images read 4h40m "stale" while containing an init that
//!   was byte-identical. The mtime moved without the content changing.
//! * This compares an image against its **source files**. Nothing relinks a
//!   `.rs` file. A source mtime moves when the file is edited or when a
//!   checkout writes a new version into the tree — which is exactly the event
//!   that makes an image stale.
//!
//! Measured on this Mac 2026-09-09, before the rule was written:
//!
//! ```text
//! kastellan/python-exec:dev  built  2026-06-26 03:54
//! workers/python-exec/src/exec/mod.rs  2026-09-03 19:15
//! workers/prelude/…                    2026-09-03 19:15
//! Cargo.lock                           2026-09-03 19:15
//! ```
//!
//! The image predates the 2026-09-02 security audit (`62d98a00`, 29 fixes)
//! and the #650 interpreter-prefix bind (`c03ec1a3`), and the four
//! `python_exec_container_e2e` tests passed against it. The rule returns
//! [`ImageAge::Stale`] on that host, which is the correct verdict.
//!
//! ⚠️ **This verdict is one-sided, and saying so is the point.** An image
//! newer than every source was *built after* them; it was not necessarily
//! built *from* them. So the fresh arm is named
//! [`ImageAge::NewerThanSources`] rather than `Fresh`: it can refute
//! staleness and it cannot certify freshness. #667's digest rule is
//! two-sided; pretending this one is would be the "certifies on partial
//! evidence" collapse that #680's review found and removed.

use std::path::PathBuf;

pub use super::container_images::{
    built_image, find_image, normalize_reference, parse_image_list, source_stamps, stamps_for,
    BuiltImage, ContainerImage, BUILT_IMAGES, PYTHON_EXEC_BUILD_INPUTS, PYTHON_EXEC_BUILD_SCRIPT,
    PYTHON_EXEC_SOURCE_DIRS,
};

/// A source file and its mtime, in seconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStamp {
    /// Repo-relative path, for the operator message.
    pub path: PathBuf,
    /// mtime, seconds since the Unix epoch.
    pub modified_unix: i64,
}

/// What the image's age says about whether it contains the code under test.
///
/// Three arms rather than #667's four: `Unusable` has no analogue here
/// because there is no image *reader* to fail — the timestamp arrives with
/// the listing that already proved the image exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageAge {
    /// A source file is newer than the image: **positive evidence** the image
    /// cannot contain the current worker.
    Stale {
        /// When the image was built.
        built_unix: i64,
        /// The newest source, i.e. the one that proves the point.
        newest: SourceStamp,
    },
    /// The image is newer than every source that could be stat'd.
    ///
    /// Consistent with freshness; **not proof of it** — see the module docs.
    /// `unstat` names sources whose mtime could not be read, so a caller can
    /// say what the verdict does not cover.
    NewerThanSources {
        /// Sources that could not be stat'd, so did not participate.
        unstat: Vec<PathBuf>,
    },
    /// No verdict is possible — no image timestamp, or no source stat'd at all.
    Indeterminate {
        /// What could not be established.
        detail: String,
    },
}

/// Compare the image's build time against the newest source mtime.
///
/// Pure over injected stamps — the seam #680's review made mandatory, after
/// finding that the impure half of the last such check could be replaced
/// wholesale with nothing failing.
pub fn image_age(
    built_unix: Option<i64>,
    sources: &[SourceStamp],
    unstat: &[PathBuf],
) -> ImageAge {
    let Some(built_unix) = built_unix else {
        return ImageAge::Indeterminate {
            detail: "the container CLI reported no build timestamp for this image".to_string(),
        };
    };
    // Order matters: the staleness check runs BEFORE the empty-source guard is
    // relevant, but the empty case must never reach the comparison — "newer
    // than every source" is vacuously true over an empty set, which would
    // certify any image on a host where the enumeration silently returned
    // nothing (#683's "green whether the loop found nothing or never ran").
    let Some(newest) = sources.iter().max_by_key(|stamp| stamp.modified_unix) else {
        return ImageAge::Indeterminate {
            detail: format!(
                "no worker source could be read, so the image's age proves nothing \
                 ({} unreadable)",
                unstat.len()
            ),
        };
    };
    if newest.modified_unix > built_unix {
        // Positive evidence outranks incomplete coverage: if any source is
        // newer, the image cannot contain it whatever else went unread.
        return ImageAge::Stale {
            built_unix,
            newest: newest.clone(),
        };
    }
    ImageAge::NewerThanSources {
        unstat: unstat.to_vec(),
    }
}

/// The workspace-local crates in `package`'s cargo dependency closure, as
/// repo-relative directories.
///
/// Test machinery: it shells out to `cargo metadata` so
/// [`PYTHON_EXEC_SOURCE_DIRS`] can be pinned against the closure cargo
/// actually computes, rather than against a second hand-written list. Under
/// `cfg(test)` for the same reason `guard` and `script_scan` are — nothing
/// outside this crate's own tests refers to it.
#[cfg(test)]
pub(crate) fn workspace_closure_dirs(package: &str) -> Result<Vec<String>, String> {
    let root = super::repo_root();
    let out = std::process::Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&root)
        .output()
        .map_err(|e| format!("could not run `cargo metadata`: {e}"))?;
    if !out.status.success() {
        return Err(format!("`cargo metadata` failed: {}", out.status));
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("`cargo metadata` output is not JSON: {e}"))?;
    let packages = meta["packages"]
        .as_array()
        .ok_or_else(|| "`cargo metadata` has no packages array".to_string())?;

    // Walk the dependency graph, keeping only workspace members. `--no-deps`
    // already restricts `packages` to the workspace, so any dependency naming
    // a package NOT in that set is a registry crate and is dropped here.
    let mut closure: Vec<String> = Vec::new();
    let mut queue = vec![package.to_string()];
    while let Some(name) = queue.pop() {
        let Some(pkg) = packages.iter().find(|p| p["name"].as_str() == Some(&name)) else {
            continue; // not a workspace member: a registry crate
        };
        if closure.contains(&name) {
            continue;
        }
        closure.push(name.clone());
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            if let Some(dep_name) = dep["name"].as_str() {
                queue.push(dep_name.to_string());
            }
        }
    }
    // Second pass: map the collected package names to their directories.
    let mut dirs: Vec<String> = Vec::new();
    for name in &closure {
        if let Some(pkg) = packages.iter().find(|p| p["name"].as_str() == Some(name)) {
            let manifest = pkg["manifest_path"].as_str().unwrap_or_default();
            if let Some(dir) = std::path::Path::new(manifest)
                .parent()
                .and_then(|d| d.strip_prefix(&root).ok())
            {
                dirs.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    Ok(dirs)
}

/// Why the `container` CLI itself cannot be used.
///
/// ⚠️ **This must never mention the image build.** Folding a CLI or
/// system-service fault into "image not present" is the #684 defect: an
/// operator whose service is stopped was sent to `build-image.sh`, and no
/// build starts a service. The remedy named here is the one that applies.
///
/// One line, for the reason [`super::probe_reason`] gives: a reason carrying
/// its own newline emits an orphan continuation line that
/// `grep -c '^\[SKIP\]'` cannot attribute to anything.
pub fn cli_unavailable_reason(detail: &str) -> String {
    format!(
        "Apple `container` is not usable on this host ({}); \
         start it with `container system start`, or install it with `brew install container`",
        crate::skip::one_line(detail)
    )
}

/// Why the requested image is absent — the one case a build *does* fix.
pub fn image_missing_reason(reference: &str) -> String {
    match built_image(reference) {
        Some(built) => format!(
            "the `{reference}` container image is not present; \
             build it with `bash {}`",
            built.build_script
        ),
        // An upstream image has no build script here, so naming one would send
        // the operator to a script that cannot produce it.
        None => format!(
            "the `{reference}` container image is not present; \
             pull it with `container image pull {reference}`"
        ),
    }
}

/// Why the image cannot contain the code under test (#687).
///
/// Names the image, the source that proves the point, and the remedy — the
/// three things #667's `stale_reason` established an operator needs, in the
/// order they need them.
pub fn stale_image_reason(
    reference: &str,
    built_unix: i64,
    newest: &SourceStamp,
    build_script: &str,
) -> String {
    format!(
        "the `{reference}` container image was built before `{}` was last changed, \
         so it cannot contain the code under test (image {}, source {}); \
         rebuild it with `bash {build_script}`",
        newest.path.display(),
        format_unix_date(built_unix),
        format_unix_date(newest.modified_unix),
    )
}

/// What the age check could **not** establish.
///
/// Rendered for both caveat arms, because "I could not read three of the
/// sources" and "I could not read the image's build time" are different
/// admissions and an operator acts on them differently.
pub fn unverified_age_reason(reference: &str, unstat: &[PathBuf]) -> String {
    if unstat.is_empty() {
        return format!(
            "could not establish whether the `{reference}` container image \
             contains the code under test; running anyway"
        );
    }
    let names: Vec<String> = unstat.iter().map(|p| p.display().to_string()).collect();
    format!(
        "the `{reference}` container image is newer than every source that could be read, \
         but {} could not be: {}",
        unstat.len(),
        names.join(", ")
    )
}

/// Why no age verdict was possible, carrying the reason the verdict itself
/// gives.
///
/// Separate from [`unverified_age_reason`] because "the image is newer than
/// everything I could read, but N of them I could not" and "I could not decide
/// at all, because X" are different admissions an operator acts on
/// differently — and because an error with no content is a defect multiplier
/// (#660/#669: three production defects once hid behind one contentless
/// `Protocol(EarlyExit)`).
pub fn indeterminate_age_reason(reference: &str, detail: &str) -> String {
    format!(
        "could not establish whether the `{reference}` container image contains the code \
         under test ({}); running anyway",
        crate::skip::one_line(detail)
    )
}

/// A Unix timestamp as a bare `YYYY-MM-DD HH:MM` UTC stamp, for messages.
///
/// Deliberately coarse: the operator needs "June, and my edit was September",
/// not a nanosecond. Falls back to the raw seconds if the value cannot be
/// represented, so a message never becomes a panic.
fn format_unix_date(unix: i64) -> String {
    jiff::Timestamp::from_second(unix)
        .map(|ts| ts.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|_| format!("unix {unix}"))
}

/// The container-tier preflight decision, with every host step injected.
///
/// Returns `true` when the caller should skip. Pure over its closures and
/// **not** cfg-gated, so the gate ORDER is unit-testable on the DGX — which
/// compiles the whole macOS container backend out. That mirrors
/// [`super::preflight`] and exists for the same measured reason: #680's
/// review found that a `cfg`-gated wiring half could be replaced with `false`
/// with nothing failing on the host that compiles it, and the mutation was not
/// even *attemptable* on the host that does not.
///
/// The order is the order an operator can act on:
///
/// 1. `probe` — is the CLI and its system service usable at all?
/// 2. `inspect` — is *this* image present, and when was it built? An `Err` is
///    a **CLI fault**; `Ok(None)` is an absent image. Conflating the two is
///    #684, which is why the two states have different types rather than
///    different values.
/// 3. `age` — can it contain the code under test? (#687)
///
/// The last is last because it is the only one that can say the run would be
/// *meaningless* rather than impossible, and the earlier steps short-circuit,
/// so a host with no `container` never pays for an inspect it cannot use.
///
/// `unmet` is the REQUIRE-aware reporter ([`super::report_unmet_microvm`]):
/// `[SKIP]` by default, panic under [`super::REQUIRE_ENV`]. `warn` is the
/// caveat path that still runs.
pub fn container_preflight(
    reference: &str,
    probe: impl FnOnce() -> Result<(), String>,
    inspect: impl FnOnce() -> Result<Option<ContainerImage>, String>,
    age: impl FnOnce(&ContainerImage, &BuiltImage) -> ImageAge,
    unmet: impl Fn(&str) -> bool,
    warn: impl Fn(&str) -> bool,
) -> bool {
    if let Err(e) = probe() {
        return unmet(&cli_unavailable_reason(&e));
    }
    let image = match inspect() {
        Ok(Some(image)) => image,
        // An inspect that cannot be READ is the CLI failing; an inspect that
        // reads cleanly and finds nothing is an absent image. Collapsing the
        // two is exactly #684, and it is why this is a `Result<Option<_>>`
        // rather than an `Option<_>`.
        Ok(None) => return unmet(&image_missing_reason(reference)),
        Err(e) => return unmet(&cli_unavailable_reason(&e)),
    };
    // An image this repo does not build has no source closure to be stale
    // against, so the age check is not merely ignored — it is never ASKED.
    // The lookup lives here, in the pure half, rather than inside the injected
    // closure: #680's review found that a decision left in the impure half
    // could be replaced wholesale with nothing failing.
    let Some(built) = built_image(reference) else {
        return false;
    };
    match age(&image, built) {
        ImageAge::NewerThanSources { unstat } if unstat.is_empty() => false,
        ImageAge::Stale { built_unix, newest } => {
            unmet(&stale_image_reason(reference, built_unix, &newest, built.build_script))
        }
        ImageAge::NewerThanSources { unstat } => warn(&unverified_age_reason(reference, &unstat)),
        ImageAge::Indeterminate { detail } => {
            warn(&indeterminate_age_reason(reference, &detail))
        }
    }
}

/// The macOS half: the calls that actually reach the `container` CLI.
///
/// ⚠️ Everything in here is invisible to the DGX, which compiles
/// `cfg(target_os = "macos")` out entirely — imports included. That is why the
/// decision logic above is *not* gated: a rule only one host can compile is a
/// rule only one host can verify, and #679 shipped an unused import through a
/// clean Mac run for exactly this reason, in the mirror direction.
#[cfg(target_os = "macos")]
mod macos {
    use super::{container_preflight, find_image, image_age, parse_image_list, ContainerImage};

    /// Look one image up: `Ok(Some)` present, `Ok(None)` absent, `Err` a CLI
    /// fault.
    ///
    /// Uses `container image inspect`, via the **production** argv producer
    /// [`kastellan_sandbox::macos_container::build_image_inspect_argv`] rather
    /// than a second spelling of the same command. #669 found the same bwrap
    /// flag pair written out in three places and two of them wrong; the lesson
    /// was to count the producers, so this crate does not become a fourth.
    ///
    /// One call yields both facts the preflight needs — presence (exit status)
    /// and build time (the JSON) — which is why it replaced the
    /// `container image list` scan the three suites used to do. That scan is
    /// also what made the substring defect possible: `inspect` takes the tag
    /// as an argument, so there is nothing to match loosely.
    pub fn inspect_image(reference: &str) -> Result<Option<ContainerImage>, String> {
        let argv = kastellan_sandbox::macos_container::build_image_inspect_argv(reference);
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("could not spawn `{}`: {e}", argv.join(" ")))?;
        // Measured: `container image inspect <absent-tag>` exits 1, and the
        // same command on a present tag exits 0 with the image's JSON. The
        // exit status is therefore the presence signal, and no output is
        // matched loosely anywhere.
        if !out.status.success() {
            return Ok(None);
        }
        let images = parse_image_list(&String::from_utf8_lossy(&out.stdout))?;
        // Re-check the name the CLI handed back rather than trusting that it
        // answered the question asked. A record whose reference does not match
        // is reported as absent, which is the fail-closed direction.
        Ok(find_image(&images, reference).cloned())
    }

    /// `[SKIP]` + `true` when the macOS container tier cannot give this run
    /// meaning — or panic when [`super::super::REQUIRE_ENV`] demanded a real
    /// micro-VM run.
    ///
    /// The single entry point the three container suites call, replacing the
    /// three byte-copied private helpers that each folded a CLI fault into
    /// "image not present" (#684) and none of which checked staleness at all
    /// (#687).
    ///
    /// # Panics
    ///
    /// Under [`crate::gliner_e2e::UnmetAction::Fail`], naming the knob and the
    /// reason.
    pub fn skip_if_no_container(reference: &str) -> bool {
        container_preflight(
            reference,
            || {
                kastellan_sandbox::macos_container::MacosContainer::probe()
                    .map_err(|e| e.to_string())
            },
            || inspect_image(reference),
            |image, built| {
                let (stamps, unstat) = super::stamps_for(built);
                image_age(image.created_unix, &stamps, &unstat)
            },
            super::super::report_unmet_microvm,
            super::super::report_caveat_microvm,
        )
    }
}

#[cfg(target_os = "macos")]
pub use macos::{inspect_image, skip_if_no_container};
