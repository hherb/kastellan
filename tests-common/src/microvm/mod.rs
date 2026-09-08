//! Micro-VM (Firecracker) test preflight: image discovery, launcher
//! discovery, and the `[SKIP]` early-returns that guard the 15 micro-VM
//! integration tests under `core/tests/` — mostly, but not all, named
//! `*firecracker*_e2e.rs`.
//!
//! # Why this module exists (issue #475)
//!
//! `skip_if_no_microvm` / `locate_microvm_run` / `image_dir` /
//! `firecracker_backend` were byte-copied into **15** integration-test
//! files. Only the rootfs filename genuinely differed between them.
//!
//! That is worse than ordinary duplication because these are the
//! **`[SKIP]` helpers**. A test that skips prints a `[SKIP]` line and
//! then *passes*, so a copy that skips for the wrong reason — or prints
//! a hint that sends the operator down the wrong path — is
//! indistinguishable from a genuinely green run. `CLAUDE.md` calls this
//! the false-green pattern; 15 copies is that pattern multiplied.
//!
//! It was not hypothetical. By the time this module was written the
//! copies had already diverged: two of the 15 told the operator to run
//! `cargo build -p kastellan-microvm-run` **without `--release`**, while
//! [`launcher_candidates`] probes `target/release` **first**. Following
//! that hint rebuilds the debug binary, leaves a stale release binary in
//! place, and silently runs *old* launcher code — the exact failure
//! recorded in the `firecracker-e2e-stale-release-launcher` note, which
//! had already cost one false bug report (#362).
//!
//! # Layout
//!
//! The pure, host-independent parts ([`build_script_for`],
//! [`launcher_candidates`], the message builders) are **not** cfg-gated,
//! so they compile and unit-test on macOS as well as Linux. Only the
//! functions that name a Firecracker type are `#[cfg(target_os =
//! "linux")]`. That split is deliberate: macOS compiles `cfg(linux)`
//! code *out*, so anything behind the gate is verified only by the DGX
//! run, and the two facts most worth protecting — the build-hint table
//! and the release-before-debug ordering — are exactly the ones that
//! need no VM to check.

use std::path::{Path, PathBuf};

/// Where the guest kernel and rootfs images live when the operator has
/// not overridden `KASTELLAN_MICROVM_DIR`.
///
/// Provisioned root-owned, group-writable (1775), with a root-owned
/// vmlinux the agent cannot replace (#479), by the one-time
/// `sudo scripts/linux/install-firecracker-vsock.sh`.
pub const DEFAULT_IMAGE_DIR: &str = "/var/lib/kastellan/microvm";

/// The VMM launcher binary. The Firecracker backend spawns this by
/// **bare name** via a `PATH` lookup, which is why
/// `skip_if_no_microvm` prepends its build directory to `PATH`.
///
/// [`launcher_skip_message`] also uses this as the `cargo build -p`
/// **package** name — true today because the crate and its binary share
/// the name. If they ever diverge, split this into two consts.
pub const LAUNCHER_BIN: &str = "kastellan-microvm-run";

/// Build profiles probed for [`LAUNCHER_BIN`], **most-preferred first**.
///
/// `release` precedes `debug` and that order is load-bearing, not
/// stylistic: if both exist the release binary wins, so a contributor
/// who rebuilds only the debug binary keeps running whatever stale code
/// is sitting in `target/release`. Every operator-facing hint in this
/// module therefore says `--release`; see the module docs.
const LAUNCHER_PROFILES: [&str; 2] = ["release", "debug"];

mod freshness;
mod images;
mod require;
pub use freshness::{
    freshness, stale_reason, unusable_reason, unverified_reason, BakedDigest, Freshness, Missing,
    Unverified,
};
pub use images::{
    baked_for, build_script_for, image_entry, BakedBinary, RootfsImage, GUEST_INIT_BIN,
    GUEST_INIT_IN_IMAGE, GUEST_KERNEL_LIB, REBUILD_ALL_SCRIPT, RELEASE_BUILD_SCRIPT,
    ROOTFS_IMAGES,
};
pub use require::{dep_or_skip, first_unmet, host_probes, skip_unless_ready, Probe};

// The source-level guard is machinery for this crate's own tests, not
// vocabulary for a suite: nothing outside `tests-common` refers to it, so it
// is compiled only under `cfg(test)` rather than shipped as public surface.
#[cfg(test)]
mod guard;

// The one shell-reading rule the build-script scanners in `images` and
// `kernel_pin_tests` share. Same reasoning as `guard`: test machinery, not
// vocabulary for a suite, so it never leaves `cfg(test)`.
#[cfg(test)]
mod script_scan;

#[cfg(test)]
mod freshness_tests;
#[cfg(test)]
mod kernel_pin_tests;
#[cfg(test)]
mod rebuild_script_tests;

/// The operator's demand that a micro-VM e2e actually *run* rather than
/// report green having skipped (issue #667, honouring #653's convention).
///
/// Same name shape and the same `env_flag_enabled` dialect as
/// [`crate::gliner_e2e::REQUIRE_ENV`]. Unset, every unmet precondition
/// prints `[SKIP]` and the test passes, which is what keeps a plain
/// `cargo test` green on a host with no KVM. Truthy, each one panics
/// naming itself.
///
/// This is not decoration. Two documented false greens on this exact path
/// were skips nobody could turn into failures: `firecracker` sitting off
/// the non-interactive ssh `PATH` (the whole suite skips-as-passes), and a
/// rootfs image too old to contain the code under test.
pub const REQUIRE_ENV: &str = "KASTELLAN_MICROVM_REQUIRE_E2E";

/// What an unmet micro-VM precondition means for *this* run.
///
/// Reads [`REQUIRE_ENV`] through the one project flag dialect
/// (`1|true|yes|on`, trimmed, case-insensitive) rather than a strict
/// `Some("1")` — the strict form is the operator-facing skew #654 was filed
/// about, and re-deriving it here would reintroduce it.
pub fn require_action() -> crate::gliner_e2e::UnmetAction {
    require_action_to(&mut std::io::stderr())
}

/// [`require_action`] with the out-of-dialect `[WARN]` written to `out`.
///
/// The warning is the half the first version dropped (#680 review): it
/// called `unmet_action` bare while its sibling
/// [`crate::gliner_e2e::require_action`] routes through the same helper and
/// *then* warns. So `KASTELLAN_MICROVM_REQUIRE_E2E=y` silently degraded to
/// `Skip` and handed back the green run the operator was trying to rule
/// out — the #654 skew, on the knob whose own doc cites #654.
pub fn require_action_to(out: &mut dyn std::io::Write) -> crate::gliner_e2e::UnmetAction {
    let raw = std::env::var(REQUIRE_ENV).ok();
    let action = crate::gliner_e2e::unmet_action(raw.clone());
    if action == crate::gliner_e2e::UnmetAction::Skip {
        crate::gliner_e2e::warn_if_out_of_dialect(REQUIRE_ENV, raw.as_deref(), out);
    }
    action
}

/// Abort the run because [`REQUIRE_ENV`] demanded one and a precondition is
/// unmet.
///
/// One renderer for a sentence that used to be written out twice, verbatim,
/// with only the `KASTELLAN_MICROVM_REQUIRE_E2E` prefix pinned by tests — so
/// the rest of it could drift silently between the two call sites (#680
/// review).
///
/// # Panics
///
/// Always. That is its whole job; the return type says so.
pub fn require_panic(reason: &str) -> ! {
    panic!(
        "{REQUIRE_ENV} demanded a real micro-VM end-to-end run, but a precondition is \
         unmet: {}",
        crate::skip::one_line(reason)
    )
}

/// Report an unmet micro-VM precondition: `[SKIP]` and return `true`, or
/// panic when the operator demanded a real run via [`REQUIRE_ENV`].
///
/// Returns `true` so callers can `return` straight out of the test, which
/// keeps every call site a one-liner and the `[SKIP]`-as-pass path
/// unchanged from before #667.
///
/// # Panics
///
/// Under [`crate::gliner_e2e::UnmetAction::Fail`], naming both the knob (so
/// the operator reads it as their own demand rather than as a regression)
/// and the reason (so they know what to fix).
pub fn report_unmet_microvm(reason: &str) -> bool {
    report_unmet_microvm_to(reason, &mut std::io::stderr())
}

/// [`report_unmet_microvm`] with the skip line written to `out`.
///
/// Exists so a unit test can prove the Skip arm **emits** the line without
/// emitting a real `[SKIP]` into the run it is protecting — asserting on
/// [`crate::skip::skip_line`] alone would leave the `eprint!` deletable with
/// the suite still green, and `grep -c '^\[SKIP\]'` is how a run is audited
/// here.
///
/// # Panics
///
/// As [`report_unmet_microvm`].
pub fn report_unmet_microvm_to(reason: &str, out: &mut dyn std::io::Write) -> bool {
    match require_action_to(out) {
        crate::gliner_e2e::UnmetAction::Fail => require_panic(reason),
        crate::gliner_e2e::UnmetAction::Skip => {
            let _ = write!(out, "{}", crate::skip::skip_line(reason));
            true
        }
    }
}

/// The repository root, derived from this crate's manifest dir.
///
/// Test-only, and shared by both child test modules — hoisted here in the
/// #667 split rather than copied, since two copies of "where is the repo"
/// is exactly the duplication this module was created to end.
#[cfg(test)]
pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .to_path_buf()
}


/// Candidate [`LAUNCHER_BIN`] paths under a workspace `target/`
/// directory, in probe order (see `LAUNCHER_PROFILES`).
///
/// Pure: it does not touch the filesystem, so a test can assert the
/// ordering without building anything.
pub fn launcher_candidates(target_dir: &Path) -> Vec<PathBuf> {
    LAUNCHER_PROFILES.iter().map(|profile| target_dir.join(profile).join(LAUNCHER_BIN)).collect()
}

/// Why the Firecracker probe refused, **without rendering a verdict**.
///
/// The `*_or_reason` shape #653 established: one caller skips on this, another
/// must fail on it (see [`REQUIRE_ENV`]), and a helper that has already
/// decided which cannot serve both.
///
/// Pure so the wording is testable. Names the rootfs (probe failures are
/// usually a missing image, and "which image?" is the operator's first
/// question) and, when known, the script that builds it.
///
/// One line, and that is a fix rather than a style choice: the hint used to be
/// appended *after* [`crate::skip::skip_line`] had flattened the reason, so it
/// emitted an orphan continuation line that no `grep -c '^\[SKIP\]'` could
/// attribute to anything — precisely the shape `skip_line`'s own doc comment
/// warns about.
pub fn probe_reason(rootfs: &str, err: &str) -> String {
    // The parenthetical names the preconditions an operator most often
    // lacks, not all of them: the probe also checks firecracker on `PATH`,
    // the guest kernel, `mkfs.ext4`, and (with confinement on) bwrap + a user
    // cgroup. `err` carries whichever actually failed, so it leads.
    let mut reason =
        format!("firecracker probe failed: {err} (needs {rootfs} + KVM + vsock, and more)");
    if let Some(script) = build_script_for(rootfs) {
        reason.push_str(&format!("; build the rootfs with: bash {script}"));
    }
    reason
}

/// The `[SKIP]` line printed when the Firecracker probe fails.
pub fn probe_skip_message(rootfs: &str, err: &str) -> String {
    crate::skip::skip_line(&probe_reason(rootfs, err))
}

/// Why the VMM launcher is unusable, **without rendering a verdict** — the
/// verdict-free half, for the same reason as [`probe_reason`].
///
/// Says `--release` deliberately: see `LAUNCHER_PROFILES`.
pub fn launcher_reason() -> String {
    format!("{LAUNCHER_BIN} not built; run `cargo build --release -p {LAUNCHER_BIN}`")
}

/// The `[SKIP]` line printed when the VMM launcher has not been built.
pub fn launcher_skip_message() -> String {
    crate::skip::skip_line(&launcher_reason())
}

/// The directory holding `vmlinux` + the rootfs images, honouring the
/// `KASTELLAN_MICROVM_DIR` override.
///
/// An empty or whitespace-only override falls back to
/// [`DEFAULT_IMAGE_DIR`] rather than resolving paths against `""`.
///
/// Returns `String` because several call sites hand it straight to
/// policy builders that take one.
pub fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE_DIR.to_string())
}

/// The workspace `target/` directory, resolved from this crate's manifest
/// dir so it is correct regardless of the caller's working directory.
pub fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .join("target")
}

/// The built [`LAUNCHER_BIN`], or `None` if neither profile has one.
pub fn locate_microvm_run() -> Option<PathBuf> {
    launcher_candidates(&workspace_target_dir()).into_iter().find(|p| p.is_file())
}

/// The `target/release/` reference copy of `name` under `target_dir`.
///
/// Pure, and a named seam rather than an inline `join`, because
/// `release` is load-bearing: every `build-*-rootfs.sh` copies out of
/// `target/release/`, so comparing against `target/debug` would compare the
/// image to a binary no image is ever built from. That mutation used to
/// survive the whole suite (#680 review).
pub fn release_binary_path(target_dir: &Path, name: &str) -> PathBuf {
    target_dir.join("release").join(name)
}

/// sha256 of `bytes`, lowercase hex.
///
/// The single producer of the digest format in this crate, which is why no
/// newtype is needed to keep the two sides comparable.
fn digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// sha256 of the working tree's copy of a binary.
///
/// Distinguishes "not built" from "built but unreadable": the first is the
/// benign common case (an operator who built only some `-p` targets), the
/// second is a host problem, and rendering a permissions error as
/// `cargo build --release` sends the operator to a command that succeeds and
/// changes nothing (#680 review).
fn target_digest(path: &Path) -> Result<String, Missing> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(digest_of(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Missing::NotBuilt),
        Err(e) => Err(Missing::Unreadable { detail: format!("{path:?}: {e}") }),
    }
}

/// The `debugfs` invocation that reads `in_image` out of `image`.
///
/// Pure so the argv is unit-testable on any host. It is worth pinning: with
/// the argv inline, mutating `-R` to `-Rx` or `cat` to `dump` left the whole
/// suite green while the check silently stopped checking (#680 review).
///
/// `cat` writes the file to **stdout**, which is what makes this safe. The
/// first version used `dump <path> <outfile>` into `$TMPDIR`, and `debugfs`
/// hands the `-R` string to libss, which splits it on whitespace — so a
/// `$TMPDIR` containing a space silently produced a malformed request and a
/// permanent `Indeterminate` for every image on that host. There is no
/// second path to quote now, and no temp file to create, race or clean up.
pub fn debugfs_argv(image: &Path, in_image: &str) -> Vec<String> {
    vec!["-R".to_string(), format!("cat {in_image}"), image.display().to_string()]
}

/// sha256 of a file *inside* an ext4 image, read without mounting it.
///
/// `debugfs -R "cat <path>"` walks the inode and writes the file's blocks to
/// stdout, so this costs one read of the file against an image that may be a
/// gigabyte, and needs no root and no loop device. `debugfs` ships with
/// `e2fsprogs`, which is already a hard prerequisite for *building* any of
/// these images (`mkfs.ext4`), so a host that can produce a rootfs can read
/// one back. Verified on the DGX that `cat` round-trips binary content
/// byte-for-byte, CR/LF and NUL included.
///
/// Every failure mode of `debugfs` exits **0** with empty stdout — a missing
/// path, an image that is not ext4, an unopenable image, a symlink — so the
/// exit status carries nothing and the emptiness of the output is the signal.
/// None of the baked binaries is zero bytes, so empty output always means the
/// read failed.
///
/// The two causes are then separated *structurally*, never by matching on
/// `debugfs`'s wording: failing to spawn it with `NotFound` is
/// [`Missing::NoImageReader`] (benign — no image on this host can be read),
/// and anything else is [`Missing::Unreadable`] carrying `debugfs`'s own
/// stderr (**not** benign — a working reader could not read *this* image).
fn image_digest(image: &Path, in_image: &str) -> Result<String, Missing> {
    let output = std::process::Command::new("debugfs")
        .args(debugfs_argv(image, in_image))
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Missing::NoImageReader)
        }
        Err(e) => return Err(Missing::Unreadable { detail: format!("debugfs: {e}") }),
    };
    if output.stdout.is_empty() {
        return Err(Missing::Unreadable { detail: debugfs_complaint(&output.stderr) });
    }
    Ok(digest_of(&output.stdout))
}

/// The last meaningful line of `debugfs`'s stderr, for the operator.
///
/// `debugfs` always announces its own version first, so the banner is
/// dropped; what is left is the actual complaint (`"File not found by
/// ext2_lookup"`, `"Filesystem not open"`). Never matched on — this is
/// operator-facing prose, and the verdict is decided structurally above.
///
/// The banner test is deliberately version-agnostic: `"debugfs 1.47.0 (…)"`
/// separates the name from the version with a **space**, while `debugfs`'s
/// own diagnostics use a **colon** (`"debugfs: …"`). Matching the version
/// digits would let a future e2fsprogs banner become somebody's diagnosis.
fn debugfs_complaint(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("debugfs ") && !l.starts_with("Using EXT2FS"))
        .unwrap_or("no output and no diagnostic from debugfs")
        .to_string()
}

/// Read every baked binary's digest from both ends and return the verdict.
///
/// The impure half of [`freshness`]; the decision itself stays pure and
/// unit-tested.
pub fn image_freshness(rootfs: &str) -> Freshness {
    let image_path = PathBuf::from(image_dir()).join(rootfs);
    let target = workspace_target_dir();
    let digests: Vec<BakedDigest> = baked_for(rootfs)
        .iter()
        .map(|b| BakedDigest {
            name: b.target_name.to_string(),
            in_image: image_digest(&image_path, b.in_image),
            in_target: target_digest(&release_binary_path(&target, b.target_name)),
        })
        .collect();
    freshness(&digests)
}

/// [`image_freshness`], computed at most once per image per process.
///
/// The verdict is deterministic for a given (image, working tree), and one
/// integration-test binary calls `skip_if_no_microvm` once per `#[test]` —
/// eight times in `python_exec_firecracker_e2e.rs` alone, each spawning
/// `debugfs` twice. Memoised for the same reason the `PATH` mutation next to
/// it is: repeated identical work in a preflight is pure cost.
fn memoised_freshness(rootfs: &str) -> Freshness {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Freshness>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // Poison-resistant: a panicking `Stale` verdict elsewhere must not wedge
    // every later suite in the same binary.
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    guard.entry(rootfs.to_string()).or_insert_with(|| image_freshness(rootfs)).clone()
}

/// Refuse to boot a rootfs image whose baked binaries differ from the ones
/// this tree builds (issue #667). Returns `true` when the caller should skip.
///
/// The four verdicts get four different treatments, and the asymmetry is the
/// design:
///
/// * [`Freshness::Fresh`] with nothing unverified — run, silently.
/// * [`Freshness::Fresh`] with something unverified — `[WARN]` and run. The
///   image matched everywhere it could be checked, but part of it was not
///   checked at all, and staying silent about that is what let a matching
///   init certify a June worker (#680 review).
/// * [`Freshness::Stale`] — **panic, unconditionally**, whatever
///   [`REQUIRE_ENV`] says. This is not an unmet precondition the operator
///   may reasonably not have; it is positive evidence that the run about to
///   happen would prove nothing about the working tree. A `[SKIP]` here
///   would be the #667 bug wearing a different hat, and these suites are
///   `#[ignore]`d, so reaching this code means an operator explicitly asked
///   for a Firecracker run.
/// * [`Freshness::Unusable`] — **panic, unconditionally**, for the same
///   reason: a reader that works elsewhere could not read this image, which
///   is positive evidence about *this* image rather than absence of
///   evidence about all of them.
/// * [`Freshness::Indeterminate`] — `[WARN]` and **run anyway**, because
///   absence of a comparable digest is not evidence of staleness and
///   downgrading a real VM run to a skip would lose coverage for no gain.
///
/// [`REQUIRE_ENV`] turns both `[WARN]` arms into failures, for an operator
/// demanding a fully-gated run.
///
/// # Panics
///
/// On [`Freshness::Stale`] and [`Freshness::Unusable`] always, and on either
/// `[WARN]` arm when [`REQUIRE_ENV`] is truthy.
pub fn skip_if_image_stale(rootfs: &str) -> bool {
    skip_if_image_stale_to(rootfs, memoised_freshness(rootfs), &mut std::io::stderr())
}

/// [`skip_if_image_stale`] with the verdict injected and the `[WARN]` line
/// written to `out`.
///
/// The seam exists so every branch is unit-testable without a rootfs image:
/// asserting on [`stale_reason`] alone would prove the wording correct while
/// leaving the panic deletable, which is the mutation that would silently
/// restore #667.
///
/// # Panics
///
/// As [`skip_if_image_stale`].
pub fn skip_if_image_stale_to(
    rootfs: &str,
    verdict: Freshness,
    out: &mut dyn std::io::Write,
) -> bool {
    let script = build_script_for(rootfs);
    match verdict {
        Freshness::Fresh { unverified } if unverified.is_empty() => false,
        Freshness::Stale { binary } => {
            panic!("{}", crate::skip::one_line(&stale_reason(rootfs, &binary, script)))
        }
        Freshness::Unusable { binary, detail } => panic!(
            "{}",
            crate::skip::one_line(&unusable_reason(rootfs, &binary, &detail, script))
        ),
        // The two caveat arms differ only in whether anything else was
        // verified; `unverified_reason` renders that distinction, and both
        // must still RUN unless the operator demanded otherwise.
        Freshness::Fresh { unverified } => warn_and_run(rootfs, &unverified, true, out),
        Freshness::Indeterminate { unverified } => warn_and_run(rootfs, &unverified, false, out),
    }
}

/// Emit the freshness caveat and let the run proceed — or fail, if the
/// operator demanded a fully-gated run.
///
/// Returns `false` (do not skip) in the `[WARN]` case: downgrading a real VM
/// run to a skip would lose coverage for nothing.
///
/// # Panics
///
/// When [`REQUIRE_ENV`] is truthy.
fn warn_and_run(
    rootfs: &str,
    unverified: &[Unverified],
    gated: bool,
    out: &mut dyn std::io::Write,
) -> bool {
    let reason = unverified_reason(rootfs, unverified, gated);
    if require_action_to(out) == crate::gliner_e2e::UnmetAction::Fail {
        require_panic(&reason);
    }
    let _ = write!(out, "{}", crate::skip::warn_line(&reason));
    false
}

/// The micro-VM preflight decision, with every host-specific step injected.
///
/// Returns `true` when the caller should skip. Pure over its closures and
/// **not** cfg-gated, so the gate ORDER is unit-testable on macOS — which
/// compiles the whole Firecracker backend out. That matters because the
/// ordering used to live only inside `#[cfg(target_os = "linux")]`: replacing
/// the freshness call with `false` left every test green on Linux, and on the
/// Mac the mutation could not even be attempted (#680 review).
///
/// The order is the order an operator can act on:
///
/// 1. `probe` — can this host boot a VM at all?
/// 2. `locate` — is the VMM launcher built?
/// 3. `on_launcher` — make it reachable (the backend spawns it by bare name).
/// 4. `stale` — does the image contain the code under test? (#667)
///
/// The fourth is last because it is the only one that can say the run would
/// be *meaningless* rather than impossible, and the only one that panics.
/// The first three short-circuit, so a host with no KVM never pays for a
/// digest it cannot use.
pub fn preflight(
    rootfs: &str,
    probe: impl FnOnce() -> Result<(), String>,
    locate: impl FnOnce() -> Option<PathBuf>,
    on_launcher: impl FnOnce(&Path),
    stale: impl FnOnce() -> bool,
    unmet: impl Fn(&str) -> bool,
) -> bool {
    if let Err(e) = probe() {
        return unmet(&probe_reason(rootfs, &e));
    }
    let Some(bin) = locate() else {
        return unmet(&launcher_reason());
    };
    on_launcher(&bin);
    stale()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::sync::Arc;

    use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
    use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};

    use super::{
        image_dir, locate_microvm_run, preflight, report_unmet_microvm, skip_if_image_stale,
    };

    /// The kernel + rootfs pair for `rootfs` (a bare filename such as
    /// `"web-fetch.ext4"`) inside [`image_dir`].
    pub fn firecracker_image_for(rootfs: &str) -> FirecrackerImage {
        let dir = std::path::PathBuf::from(image_dir());
        FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join(rootfs) }
    }

    /// Make the built launcher reachable by bare name, once per process.
    ///
    /// A process-global mutation, but each integration-test binary is its own
    /// process and the `Once` makes repeated calls idempotent. Hoisting the
    /// 15 copies into one shared `Once` is strictly better than 15
    /// independent ones.
    fn prepend_launcher_to_path(bin: &Path) {
        use std::sync::Once;
        static PATH_ONCE: Once = Once::new();
        PATH_ONCE.call_once(|| {
            let dir = bin.parent().expect("launcher path has a parent").to_path_buf();
            let cur = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![dir];
            paths.extend(std::env::split_paths(&cur));
            let joined = std::env::join_paths(paths).expect("join PATH");
            std::env::set_var("PATH", joined);
        });
    }

    /// Returns `true` if this host cannot boot `rootfs`, after printing a
    /// `[SKIP]` line saying which prerequisite is missing. Callers
    /// `return` immediately.
    ///
    /// A thin adapter over [`preflight`], which holds the gate ordering and
    /// is unit-tested on every host including macOS. Everything Linux-only
    /// lives in the four closures below, so nothing that can be tested
    /// portably is trapped behind the `cfg`.
    ///
    /// With VMM confinement on (`KASTELLAN_MICROVM_CONFINE_VMM` unset — the
    /// default), the probe *also* fails closed on a missing bwrap or user
    /// cgroup (the slice-5a gate), so a host without the AppArmor profile or
    /// a systemd user session `[SKIP]`s here too — read the probe error
    /// before assuming a KVM/vsock problem. The probe's own message names
    /// more preconditions than the reason string's parenthetical does.
    ///
    /// # Panics
    ///
    /// When the image is stale or unreadable (always), or when any
    /// precondition is unmet and [`super::REQUIRE_ENV`] is truthy — see
    /// [`skip_if_image_stale`].
    pub fn skip_if_no_microvm(rootfs: &str) -> bool {
        preflight(
            rootfs,
            || LinuxFirecracker::probe(&firecracker_image_for(rootfs)).map_err(|e| e.to_string()),
            locate_microvm_run,
            prepend_launcher_to_path,
            || skip_if_image_stale(rootfs),
            report_unmet_microvm,
        )
    }

    /// The Firecracker micro-VM backend, resolved through the same
    /// registry production uses.
    pub fn firecracker_backend() -> Arc<dyn SandboxBackend> {
        SandboxBackends::default_for_current_os()
            .resolve(Some(SandboxBackendKind::FirecrackerVm), None)
    }
}

#[cfg(target_os = "linux")]
pub use linux::{firecracker_backend, firecracker_image_for, skip_if_no_microvm};

#[cfg(test)]
mod preflight_tests;

#[cfg(test)]
mod call_site_tests;
#[cfg(test)]
mod guard_tests;
#[cfg(test)]
mod require_tests;
