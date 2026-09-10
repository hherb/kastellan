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
//!   problem no build can fix. [`cli_unavailable_reason`] and
//!   [`image_missing_reason`] separate the two.
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
    PYTHON_EXEC_IMAGE, PYTHON_EXEC_SOURCE_DIRS,
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

/// How much a build time may lead this host's clock before it is disbelieved.
///
/// Generous, because small skew between a build host and this one is normal
/// and is not what this arm is for; an image from the future by more than an
/// hour is a broken clock, not a rounding difference.
pub const FUTURE_BUILD_SLACK_SECS: i64 = 3600;

/// Compare the image's build time against the newest source mtime.
///
/// Pure over injected stamps **and over `now_unix`** — the seam #680's review
/// made mandatory, after finding that the impure half of the last such check
/// could be replaced wholesale with nothing failing. Taking the clock as a
/// parameter is what makes the future-build arm testable at all.
pub fn image_age(
    built_unix: Option<i64>,
    sources: &[SourceStamp],
    unstat: &[PathBuf],
    now_unix: i64,
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
    // ⚠️ An image that claims to have been built in the FUTURE is more
    // suspicious than one built yesterday, not less — and a bare `>` silently
    // certifies it forever. Clock skew is real here: the image is built inside
    // a container on a cross-build host, and `container image save`/`load`
    // moves images between machines. This is the one way the one-sided rule
    // could quietly do the certifying it says it cannot do.
    if built_unix > now_unix.saturating_add(FUTURE_BUILD_SLACK_SECS) {
        return ImageAge::Indeterminate {
            detail: format!(
                "the image's build time ({}) is in the future relative to this host ({}), \
                 so a clock is wrong somewhere and the age comparison proves nothing",
                format_unix_date(built_unix),
                format_unix_date(now_unix)
            ),
        };
    }
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
    let mut cmd = std::process::Command::new(env!("CARGO"));
    cmd.args(["metadata", "--no-deps", "--format-version", "1"]).current_dir(&root);
    // BOUNDED-EXEMPT: `cargo metadata` takes cargo's own package-cache lock,
    // which a concurrent build on the same machine may legitimately hold for
    // minutes. A budget here would turn a benign wait into a flake, which is
    // the opposite of #690's purpose — the point is to make *unbounded*
    // failures legible, not to fail on slow-but-correct ones.
    let out = cmd
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

/// Why the `container` CLI could not answer the question asked — whether that
/// question was `system status` or `image inspect`.
///
/// Three variants, because the three faults have **different remedies** and
/// folding any two is the same mistake as folding a CLI fault into an absent
/// image (#684). A service that is down is fixed by starting it; one that is
/// *wedged* needs a `stop` first, so the same sentence is the wrong advice; and
/// a CLI whose output this gate can no longer read is fixed by changing this
/// gate, where telling the operator to restart a service would waste their
/// time exactly as `build-image.sh` did.
///
/// One type serves both the probe and the inspect deliberately: they can fail
/// the same three ways, and two near-identical enums would be the drift
/// channel #669 spent a session closing. The probe never parses output, so it
/// simply never produces [`CliFault::Unreadable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliFault {
    /// The CLI could not be spawned, died on a signal, or exited with a
    /// status this gate does not recognise. Remedy: the service.
    Unavailable(String),
    /// The CLI was spawned and never answered inside its budget, and was
    /// killed (#690). Remedy: unwedge the daemon — which is `stop` **then**
    /// `start`, not `start`, so it is **not** the remedy above.
    ///
    /// ⚠️ Before #690 this variant had no observable form at all: the process
    /// simply never returned, and `cargo test --workspace` stalled with no
    /// `[SKIP]`, no `[WARN]`, no panic and no message of any kind. Naming it
    /// is the whole point — an error with no content is a defect multiplier.
    Wedged(String),
    /// The CLI answered and the answer could not be read. Remedy: this gate.
    Unreadable(String),
}

/// What a finished `container image inspect` says, before its stdout is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectExit {
    /// Exit 0 — the image is present and its JSON is on stdout.
    Present,
    /// Exit 1 — the CLI read the image store cleanly, and the tag is not in
    /// it. The one case a build or a pull actually fixes.
    Absent,
}

/// The exit code Apple `container` uses for "this tag is not in the store".
///
/// ⚠️ Exit 1 is the absence signal and it is the **only** one. Measured on
/// Apple `container` 1.1.0: an absent tag exits 1, a present tag exits 0, and
/// a missing argument exits **64**. Treating every non-zero status as absence
/// is #684's folding defect re-entering through the exit status instead of
/// through the spawn — it sends an operator whose CLI changed under them to a
/// ten-minute cross-build that cannot possibly help.
const ABSENT_IMAGE_EXIT: i32 = 1;

/// Classify a finished `container image inspect` by its exit status.
///
/// Pure, and deliberately **not** `cfg`-gated, so the classification is
/// unit-testable on a host with no Apple `container` at all — the seam that
/// makes the fault arms reachable. Before it existed, the only broken-inspect
/// shape any test could reach was the one that arrives through
/// [`parse_image_list`], while the exit-status arm was covered by nothing and
/// reported as covered [[unreachable-success-path-proves-nothing]].
///
/// `stderr` rides along in the reason rather than being discarded: an error
/// with no content is a defect multiplier (#660/#669).
pub fn classify_inspect_exit(
    code: Option<i32>,
    stderr: &str,
    command: &str,
) -> Result<InspectExit, CliFault> {
    match code {
        Some(0) => Ok(InspectExit::Present),
        Some(ABSENT_IMAGE_EXIT) => Ok(InspectExit::Absent),
        Some(other) => Err(CliFault::Unavailable(format!(
            "`{command}` exited {other}{}",
            trailing_detail(stderr)
        ))),
        // No code at all means a signal killed it, which is never an absent
        // image and must not be reported as one.
        None => Err(CliFault::Unavailable(format!(
            "`{command}` was killed by a signal{}",
            trailing_detail(stderr)
        ))),
    }
}

/// `": <one-line stderr>"`, or nothing when the process said nothing.
fn trailing_detail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {}", crate::skip::one_line(trimmed))
    }
}

/// Why the CLI answered in a shape this gate cannot read.
///
/// ⚠️ **Not** [`cli_unavailable_reason`]: no `container system start` and no
/// image build fixes a schema change, and both were what the operator used to
/// be told. The remedy named here is the only one that applies — the gate.
pub fn unreadable_inspect_reason(reference: &str, detail: &str) -> String {
    format!(
        "the `container` CLI answered for `{reference}` in a shape this freshness gate \
         cannot read ({}); the gate needs updating — no service restart and no image \
         build will change it",
        crate::skip::one_line(detail)
    )
}

/// Why the `container` CLI was killed without answering (#690).
///
/// ⚠️ **Not** [`cli_unavailable_reason`], and the difference is the remedy, not
/// the tone: that sentence ends in `container system start`, which is what you
/// do to a *stopped* service and not enough for a *wedged* one. Appending it
/// after a correct remedy is the #684 folding defect in its politest form —
/// the right advice is still there, with weaker advice after it.
///
/// The detail already names the command, the budget and the remedy (it comes
/// from [`kastellan_sandbox::bounded_command::timed_out_reason`]), so this
/// wrapper only has to place it and add nothing that contradicts it.
///
/// One line, for the reason [`super::probe_reason`] gives: a reason carrying
/// its own newline emits an orphan continuation line that
/// `grep -c '^\[SKIP\]'` cannot attribute to anything.
pub fn wedged_cli_reason(detail: &str) -> String {
    format!(
        "Apple `container` did not answer on this host ({})",
        crate::skip::one_line(detail)
    )
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

/// The operator-facing sentence for a [`CliFault`] — the **one** place a fault
/// becomes prose.
///
/// Pure, exhaustive, and shared by both the probe arm and the inspect arm of
/// [`container_preflight`], so a fourth fault cannot be added with one of the
/// two arms quietly left rendering it as something else. Before #690 the two
/// arms rendered independently and already disagreed: the probe arm sent every
/// fault through [`cli_unavailable_reason`], including ones it does not fit.
pub fn cli_fault_reason(reference: &str, fault: &CliFault) -> String {
    match fault {
        CliFault::Unavailable(detail) => cli_unavailable_reason(detail),
        CliFault::Wedged(detail) => wedged_cli_reason(detail),
        CliFault::Unreadable(detail) => unreadable_inspect_reason(reference, detail),
    }
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

/// This host's wall clock, seconds since the Unix epoch.
///
/// The single impure input to the age rule, injected at the one call site so
/// [`image_age`] stays pure. A clock before the epoch reads as 0, which is
/// only reachable on a host whose clock is already unusable.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
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
/// 2. `inspect` — is *this* image present, and when was it built? `Ok(None)`
///    is an absent image; an `Err` is a fault, and [`CliFault`] splits it
///    again into "the CLI could not answer" and "the CLI answered and this
///    gate could not read it". Conflating any of the three is #684, which is
///    why they are different types rather than different values.
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
    probe: impl FnOnce() -> Result<(), CliFault>,
    inspect: impl FnOnce() -> Result<Option<ContainerImage>, CliFault>,
    age: impl FnOnce(&ContainerImage, &BuiltImage) -> ImageAge,
    unmet: impl Fn(&str) -> bool,
    warn: impl Fn(&str) -> bool,
) -> bool {
    if let Err(fault) = probe() {
        return unmet(&cli_fault_reason(reference, &fault));
    }
    let image = match inspect() {
        Ok(Some(image)) => image,
        // An inspect that cannot be READ is the CLI failing; an inspect that
        // reads cleanly and finds nothing is an absent image. Collapsing the
        // two is exactly #684, and it is why this is a `Result<Option<_>>`
        // rather than an `Option<_>`.
        Ok(None) => return unmet(&image_missing_reason(reference)),
        // The two faults have different remedies, so they get different
        // messages. Folding them would repeat #684 one layer up: a schema
        // change is not fixed by starting a service, any more than a stopped
        // service is fixed by building an image.
        Err(fault) => return unmet(&cli_fault_reason(reference, &fault)),
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
        // ⚠️ The Firecracker twin PANICS here unconditionally, and this tier
        // deliberately does not. That rule's written justification is "these
        // suites are `#[ignore]`d, so reaching this code means an operator
        // explicitly asked for a VM run" — and that premise is FALSE here:
        // all eight container tests run on a plain `cargo test --workspace`.
        // So the same panic would turn every sweep on every Mac with the CLI
        // installed red on a stale image. Routing through `unmet` keeps both
        // behaviours instead: `[SKIP]` by default, panic under REQUIRE.
        // Operator's decision, 2026-09-09; pinned by
        // `a_stale_image_is_an_unmet_precondition_not_a_warning`.
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
    use super::{
        classify_inspect_exit, container_preflight, find_image, image_age, parse_image_list,
        ContainerImage, InspectExit, CliFault,
    };

    /// Look one image up: `Ok(Some)` present, `Ok(None)` absent, `Err` a
    /// fault — [`CliFault::Unavailable`] when the CLI could not answer,
    /// [`CliFault::Unreadable`] when it answered unintelligibly.
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
    pub fn inspect_image(reference: &str) -> Result<Option<ContainerImage>, CliFault> {
        let argv = kastellan_sandbox::macos_container::build_image_inspect_argv(reference);
        let command = argv.join(" ");
        // Bounded (#690): Apple `container` is daemon-backed, so a wedged
        // apiserver makes this BLOCK rather than fail. Unbounded, that stalled
        // a whole `cargo test --workspace` with no output at all.
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let out = kastellan_sandbox::bounded_command::probe_output(
            &mut cmd,
            kastellan_sandbox::bounded_command::PROBE_BUDGET,
        )
        .map_err(|failure| match failure {
            kastellan_sandbox::bounded_command::ProbeFailure::Wedged(t) => {
                CliFault::Wedged(kastellan_sandbox::bounded_command::timed_out_reason(
                    &command,
                    &t,
                    kastellan_sandbox::macos_container::WEDGED_APISERVER_HINT,
                ))
            }
            kastellan_sandbox::bounded_command::ProbeFailure::Spawn(e) => {
                CliFault::Unavailable(format!("could not spawn `{command}`: {e}"))
            }
        })?;
        // The exit status is the presence signal, and ONLY exit 1 means
        // absence — see `ABSENT_IMAGE_EXIT`. The classification is a pure
        // function so its fault arms are reachable from a unit test on any
        // host; this wiring must stay thin enough to have nothing of its own
        // to get wrong.
        let stderr = String::from_utf8_lossy(&out.stderr);
        match classify_inspect_exit(out.status.code(), &stderr, &command)? {
            InspectExit::Absent => return Ok(None),
            InspectExit::Present => {}
        }
        // A CLI that answered but cannot be parsed is `Unreadable`, never a
        // service fault: the remedy is this gate, not `container system start`.
        let images = parse_image_list(&String::from_utf8_lossy(&out.stdout))
            .map_err(CliFault::Unreadable)?;
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
            // `probe_fault`, not `probe`: the flattened `SandboxError` could
            // only be re-classified by matching on its prose, which is what
            // this module refuses to do anywhere else (#690).
            || {
                kastellan_sandbox::macos_container::MacosContainer::probe_fault().map_err(
                    |fault| match fault {
                        kastellan_sandbox::macos_container::ProbeFault::Wedged(reason) => {
                            CliFault::Wedged(reason)
                        }
                        kastellan_sandbox::macos_container::ProbeFault::Unavailable(reason) => {
                            CliFault::Unavailable(reason)
                        }
                    },
                )
            },
            || inspect_image(reference),
            |image, built| {
                let (stamps, unstat) = super::stamps_for(built);
                image_age(image.created_unix, &stamps, &unstat, super::now_unix())
            },
            super::super::report_unmet_microvm,
            super::super::report_caveat_microvm,
        )
    }
}

#[cfg(target_os = "macos")]
pub use macos::{inspect_image, skip_if_no_container};
