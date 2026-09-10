//! The container-image registry, the CLI's listing format, and the source
//! closure a staleness verdict is measured against (#684, #687).
//!
//! Split out of [`super::container`] to keep both files inside the project's
//! 500-line rule, along the seam the Firecracker side already uses: `images`
//! holds *what exists and where it came from*, while its sibling holds *what
//! that means for this run*.
//!
//! The load-bearing idea here is [`BUILT_IMAGES`]. A staleness rule needs a
//! source closure, and only an image **this repo builds** has one — which is
//! why `alpine:3.20` carries no verdict rather than a wrong one.

use std::path::{Path, PathBuf};

use super::container::SourceStamp;

/// One image as the `container` CLI reports it.
///
/// `created` is optional because the CLI emits image records whose variants
/// carry no `created` field at all (measured: every multi-arch manifest here
/// has `"architecture":"unknown"` variants with `created: None`). An absent
/// timestamp is `ImageAge::Indeterminate` (declared in the sibling module,
/// so a plain span rather than an unresolvable link), never a silent pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerImage {
    /// The normalised reference, e.g. `kastellan/python-exec:dev`.
    pub reference: String,
    /// Image build time, seconds since the Unix epoch.
    pub created_unix: Option<i64>,
}

/// The workspace source directories baked into the python-exec image.
///
/// The cargo dependency closure of `kastellan-worker-python-exec` restricted
/// to workspace-local crates, measured with `cargo metadata`. ⚠️ **A
/// hand-maintained copy of a closure the build computes is exactly the drift
/// this tree keeps getting bitten by**, so it is pinned against `cargo
/// metadata` itself by `the_source_closure_matches_cargo_metadata` — the same
/// "follow the real thing" move that made #682's build-script pin follow the
/// `source` into `lib/guest-kernel.sh`.
pub const PYTHON_EXEC_SOURCE_DIRS: [&str; 3] =
    ["protocol", "workers/prelude", "workers/python-exec"];

/// A container image **this repo builds**, and what it is built from.
///
/// The registry is what makes a staleness verdict meaningful, and its absence
/// is what makes one meaningless: `alpine:3.20` is pulled from upstream, has
/// no source in this tree, and cannot be "rebuilt" by any script here. The
/// first version of this module applied the python-exec closure to every
/// reference and told an operator to rebuild alpine with `build-image.sh` —
/// caught by `lifecycle_container_routing_e2e` on the real host.
///
/// Directly parallel to #667's `ROOTFS_IMAGES`, which records what each
/// Firecracker image bakes and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltImage {
    /// The image reference, as the daemon asks for it.
    pub reference: &'static str,
    /// Workspace directories whose sources are compiled into the image.
    pub source_dirs: &'static [&'static str],
    /// Non-crate files whose change also invalidates it.
    pub build_inputs: &'static [&'static str],
    /// The script that rebuilds it — the remedy every message names.
    pub build_script: &'static str,
}

/// The python-exec image reference, taken from the daemon rather than copied.
///
/// ⚠️ **This is the constant the three suites actually pass**, not a second
/// spelling of it. A hand-written copy here would have been the whole gate's
/// single point of silent failure: [`built_image`] looks the reference up,
/// an unregistered one is never age-checked, and that arm returns without a
/// `[SKIP]` or a `[WARN]` — so bumping the daemon's tag would have deleted
/// the #687 gate from all three suites with **no change to any test output**
/// and every assertion still green. Naming the real constant makes that
/// drift a compile-time impossibility rather than a test we hope catches it.
pub const PYTHON_EXEC_IMAGE: &str = kastellan_core::workers::python_exec::DEFAULT_IMAGE;

/// Every container image this repo builds.
///
/// One entry today. It is a table rather than three constants so a second
/// worker image (the gliner-relex image is the obvious next one) is a row,
/// not a second copy of the rule.
pub const BUILT_IMAGES: &[BuiltImage] = &[BuiltImage {
    reference: PYTHON_EXEC_IMAGE,
    source_dirs: &PYTHON_EXEC_SOURCE_DIRS,
    build_inputs: &PYTHON_EXEC_BUILD_INPUTS,
    build_script: PYTHON_EXEC_BUILD_SCRIPT,
}];

/// The registry entry for `reference`, or `None` when this repo does not build
/// that image — in which case **no staleness rule applies**.
pub fn built_image(reference: &str) -> Option<&'static BuiltImage> {
    let wanted = normalize_reference(reference);
    BUILT_IMAGES
        .iter()
        .find(|image| normalize_reference(image.reference) == wanted)
}

/// Non-crate files whose change also invalidates the image.
///
/// `Cargo.lock` because the cross-build runs `--locked`, so a dependency bump
/// changes the baked binary without touching any file under
/// [`PYTHON_EXEC_SOURCE_DIRS`]; the build script because it decides the base
/// image, the build image and the copy. (`workers/python-exec/Containerfile`
/// needs no entry — it already lives under one of the source dirs.)
pub const PYTHON_EXEC_BUILD_INPUTS: [&str; 2] =
    ["Cargo.lock", "scripts/workers/python-exec/build-image.sh"];

/// The registry prefix the CLI stores Docker Hub images under.
///
/// Measured: `container image list --format json` reports `alpine:3.20` as
/// `docker.io/library/alpine:3.20` but a locally-built
/// `kastellan/python-exec:dev` under its bare name. A caller asks for the
/// name it built or pulled, so both sides are normalised before comparison.
const DOCKER_HUB_PREFIX: &str = "docker.io/library/";

/// Strip the implicit Docker Hub prefix so a caller's reference and the CLI's
/// can be compared.
///
/// Deliberately **non-widening**: it removes one exact known prefix and
/// changes nothing else, so `python-exec` still does not match
/// `kastellan/python-exec:dev`. Widening a fixture matcher is how the defect
/// in this module's docs got in.
pub fn normalize_reference(reference: &str) -> &str {
    reference.strip_prefix(DOCKER_HUB_PREFIX).unwrap_or(reference)
}

/// Parse Apple `container`'s image-record JSON into image records.
///
/// ⚠️ **The production caller feeds this `container image inspect <tag>`**,
/// not `container image list --format json`. Both emit the same array of
/// `configuration.name` / `configuration.creationDate` records (measured on
/// 1.1.0, which is why one parser serves both), but the fixtures in this
/// crate's tests are `list` captures — so nothing here pins the shape the
/// production path actually parses. Treat that as the gap it is: a fixture
/// that certifies a neighbouring command is the shape this branch exists to
/// kill [[stale-fixture-turns-a-gate-into-a-formality]].
///
/// A real JSON parse rather than a line scan, for the reason `installable.rs`
/// states: this guard's failure mode is a silent false pass, and a
/// `contains()` over CLI output is precisely the defect being fixed.
pub fn parse_image_list(json: &str) -> Result<Vec<ContainerImage>, String> {
    let root: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("the `container` CLI's image-record output is not JSON: {e}"))?;
    let entries = root
        .as_array()
        .ok_or_else(|| "expected a JSON array of images".to_string())?;
    let parsed: Vec<ContainerImage> = entries
        .iter()
        .filter_map(|entry| {
            let configuration = entry.get("configuration")?;
            let name = configuration.get("name")?.as_str()?;
            Some(ContainerImage {
                reference: normalize_reference(name).to_string(),
                created_unix: configuration
                    .get("creationDate")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_rfc3339_seconds),
            })
        })
        .collect();
    // ⚠️ Records the CLI sent that NONE of which carried the shape this reads
    // is a schema change, not an empty store — and the two used to be the same
    // value. An empty vec reaches `find_image`, returns `None`, and renders as
    // "the image is not present; build it": a parse failure wearing the
    // absent-image costume, which is the #684 folding one layer down.
    if !entries.is_empty() && parsed.is_empty() {
        return Err(format!(
            "the CLI returned {} image record(s) and none carried the `configuration.name` \
             this gate reads; its output shape has changed",
            entries.len()
        ));
    }
    Ok(parsed)
}

/// An RFC 3339 instant as seconds since the Unix epoch, or `None`.
///
/// `None` rather than an error: a record the CLI reports without a usable
/// timestamp is `ImageAge::Indeterminate`, which is a verdict the caller
/// already has to render. Turning it into a parse failure would conflate "this
/// image has no build time" with "the CLI is broken" — the folding this module
/// exists to undo.
fn parse_rfc3339_seconds(raw: &str) -> Option<i64> {
    raw.parse::<jiff::Timestamp>().ok().map(|ts| ts.as_second())
}

/// The image matching `reference` exactly, after normalising both sides.
pub fn find_image<'a>(images: &'a [ContainerImage], reference: &str) -> Option<&'a ContainerImage> {
    let wanted = normalize_reference(reference);
    images.iter().find(|image| image.reference == wanted)
}

/// The script that builds the python-exec container image.
///
/// One spelling, referenced by every message that names it, so the two
/// remedies cannot drift apart the way the byte-copied `[SKIP]` helpers this
/// module replaces did.
pub const PYTHON_EXEC_BUILD_SCRIPT: &str = "scripts/workers/python-exec/build-image.sh";

/// mtimes for every source the python-exec image is built from.
///
/// Returns `(readable, unreadable)`: a path whose mtime cannot be read does
/// **not** silently vanish, because a check that quietly stops checking is the
/// class #680's review removed from the Firecracker side. The caller carries
/// the unreadable list into the verdict.
///
/// Not cfg-gated: it walks a checkout, which both hosts have, so the DGX
/// verifies the same enumeration macOS does. Only the `container` CLI calls
/// are macOS-only.
///
/// `target/` is pruned. Including build output would swamp the comparison with
/// artefacts whose mtimes move on every build — reintroducing the very relink
/// problem that made mtime the wrong rule for #667.
///
/// ⚠️ **Convenience for tests, despite being `pub`.** Every caller today is in
/// this crate's own test modules; the preflight calls [`stamps_for`] with the
/// row [`built_image`] returned, because it has already looked one up. Kept
/// because the walk is worth exercising on both hosts by name.
pub fn source_stamps() -> (Vec<SourceStamp>, Vec<PathBuf>) {
    // By name, not by index: `BUILT_IMAGES` is documented as a table a second
    // row can be added to, and a row inserted at 0 would silently change what
    // this function means while its doc kept saying python-exec.
    let image = built_image(PYTHON_EXEC_IMAGE)
        .expect("the python-exec image is registered in BUILT_IMAGES");
    stamps_for(image)
}

/// mtimes for one registered image's source closure.
pub fn stamps_for(image: &BuiltImage) -> (Vec<SourceStamp>, Vec<PathBuf>) {
    let root = repo_root_for_sources();
    let mut stamps = Vec::new();
    let mut unstat = Vec::new();
    for dir in image.source_dirs {
        collect_dir(&root, &root.join(dir), &mut stamps, &mut unstat);
    }
    for input in image.build_inputs {
        push_stamp(&root, &root.join(input), &mut stamps, &mut unstat);
    }
    (stamps, unstat)
}

/// The workspace root this crate lives in.
///
/// A sibling of the `cfg(test)`-only `super::repo_root`, because
/// [`stamps_for`] runs on the suites' path — `skip_if_no_container` calls it
/// per run — and so must exist outside `cfg(test)`.
///
/// ⚠️ This used to credit [`source_stamps`] for that, which is **not** true:
/// the production path calls [`stamps_for`] with the registry row it looked
/// up, and `source_stamps` has only ever had test callers. The requirement is
/// real, the function named for it was wrong.
fn repo_root_for_sources() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .to_path_buf()
}

/// Walk `dir`, recording every file's mtime, pruning `target/`.
fn collect_dir(root: &Path, dir: &Path, stamps: &mut Vec<SourceStamp>, unstat: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            unstat.push(relative(root, dir));
            return;
        }
    };
    for entry in entries {
        // ⚠️ NOT `.flatten()`. A `DirEntry` that cannot be read is exactly
        // what this function's contract promises never to lose: `.flatten()`
        // dropped it from BOTH lists, so the file the gate could no longer
        // see was also the file it stopped admitting it could not check.
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                unstat.push(relative(root, dir));
                continue;
            }
        };
        let path = entry.path();
        // ⚠️ `entry.file_type()`, not `path.is_dir()`: the latter FOLLOWS
        // symlinks, so a link pointing at an ancestor recurses until the stack
        // runs out. A source tree has no reason to contain one, which is
        // exactly why nobody would notice adding one.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            // Build output has nothing to say about source freshness.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_dir(root, &path, stamps, unstat);
        } else {
            push_stamp(root, &path, stamps, unstat);
        }
    }
}

/// Record one file's mtime, or note it as unreadable.
fn push_stamp(root: &Path, path: &Path, stamps: &mut Vec<SourceStamp>, unstat: &mut Vec<PathBuf>) {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        unstat.push(relative(root, path));
        return;
    };
    // ⚠️ An mtime that cannot be turned into a timestamp goes to `unstat`, it
    // does NOT become a synthetic one. Substituting 0 here used to keep the
    // file in the comparison at the one value guaranteed never to trigger
    // `Stale`, and kept it out of `unstat` too — so the gate silently voted
    // "fresh" on a file whose age it had failed to establish. A pre-epoch or
    // out-of-range mtime is a fact about a file the check could not read.
    match modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
    {
        Some(secs) => stamps.push(SourceStamp {
            path: relative(root, path),
            modified_unix: secs,
        }),
        None => unstat.push(relative(root, path)),
    }
}

/// `path` relative to the repo root, for operator-facing messages.
fn relative(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}
