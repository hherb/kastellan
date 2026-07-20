//! Micro-VM (Firecracker) test preflight: image discovery, launcher
//! discovery, and the `[SKIP]` early-returns that guard every
//! `*_firecracker_*_e2e.rs` integration test.
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
/// Provisioned (created + chowned to the worker user) by the one-time
/// `sudo scripts/linux/install-firecracker-vsock.sh`.
pub const DEFAULT_IMAGE_DIR: &str = "/var/lib/kastellan/microvm";

/// The VMM launcher binary. The Firecracker backend spawns this by
/// **bare name** via a `PATH` lookup, which is why
/// [`skip_if_no_microvm`] prepends its build directory to `PATH`.
pub const LAUNCHER_BIN: &str = "kastellan-microvm-run";

/// Build profiles probed for [`LAUNCHER_BIN`], **most-preferred first**.
///
/// `release` precedes `debug` and that order is load-bearing, not
/// stylistic: if both exist the release binary wins, so a contributor
/// who rebuilds only the debug binary keeps running whatever stale code
/// is sitting in `target/release`. Every operator-facing hint in this
/// module therefore says `--release`; see the module docs.
const LAUNCHER_PROFILES: [&str; 2] = ["release", "debug"];

/// Every rootfs image the e2e suite boots, paired with the script that
/// builds it (repo-relative).
///
/// This is an explicit table rather than a derived
/// `build-<stem>-rootfs.sh` convention because two entries break that
/// convention and a derived name would produce a hint pointing at a file
/// that does not exist:
///
/// * `python-exec.ext4` is built by plain `build-rootfs.sh` (it was the
///   first rootfs, before the per-worker naming settled), and
/// * `kv-demo.ext4`'s script lives under `scripts/workers/kv-demo/`,
///   not `scripts/workers/microvm/` like every other one.
///
/// `every_build_script_exists` pins the whole table against the working
/// tree, so renaming or moving a script fails the unit test instead of
/// silently misleading whoever hits the `[SKIP]`.
const ROOTFS_BUILD_SCRIPTS: &[(&str, &str)] = &[
    ("python-exec.ext4", "scripts/workers/microvm/build-rootfs.sh"),
    ("web-fetch.ext4", "scripts/workers/microvm/build-web-fetch-rootfs.sh"),
    ("web-search.ext4", "scripts/workers/microvm/build-web-search-rootfs.sh"),
    ("web-research.ext4", "scripts/workers/microvm/build-web-research-rootfs.sh"),
    ("browser-driver.ext4", "scripts/workers/microvm/build-browser-driver-rootfs.sh"),
    ("matrix.ext4", "scripts/workers/microvm/build-matrix-rootfs.sh"),
    ("net-demo.ext4", "scripts/workers/microvm/build-net-demo-rootfs.sh"),
    ("kv-demo.ext4", "scripts/workers/kv-demo/build-kv-demo-rootfs.sh"),
];

/// The build script for `rootfs`, or `None` for an image this table does
/// not know about.
///
/// Pure — no filesystem access, so it is unit-testable on any host.
/// Callers fold the `None` case into a generic hint rather than guessing
/// a filename; a guessed hint is the failure mode this module exists to
/// prevent.
pub fn build_script_for(rootfs: &str) -> Option<&'static str> {
    ROOTFS_BUILD_SCRIPTS.iter().find(|(name, _)| *name == rootfs).map(|(_, script)| *script)
}

/// Candidate [`LAUNCHER_BIN`] paths under a workspace `target/`
/// directory, in probe order (see [`LAUNCHER_PROFILES`]).
///
/// Pure: it does not touch the filesystem, so a test can assert the
/// ordering without building anything.
pub fn launcher_candidates(target_dir: &Path) -> Vec<PathBuf> {
    LAUNCHER_PROFILES.iter().map(|profile| target_dir.join(profile).join(LAUNCHER_BIN)).collect()
}

/// The `[SKIP]` line printed when the Firecracker probe fails.
///
/// Pure so the wording is testable. Names the rootfs (probe failures are
/// usually a missing image, and "which image?" is the operator's first
/// question) and, when known, the script that builds it.
pub fn probe_skip_message(rootfs: &str, err: &str) -> String {
    let mut msg = format!("\n[SKIP] firecracker probe failed (need {rootfs} + KVM + vsock): {err}\n");
    if let Some(script) = build_script_for(rootfs) {
        msg.push_str(&format!("       build the rootfs with: bash {script}\n"));
    }
    msg
}

/// The `[SKIP]` line printed when the VMM launcher has not been built.
///
/// Says `--release` deliberately: see [`LAUNCHER_PROFILES`].
pub fn launcher_skip_message() -> String {
    format!("\n[SKIP] {LAUNCHER_BIN} not built; run `cargo build --release -p {LAUNCHER_BIN}`\n")
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

/// The built [`LAUNCHER_BIN`], or `None` if neither profile has one.
///
/// Resolves the workspace `target/` from this crate's manifest dir, so
/// it is correct regardless of the caller's working directory.
pub fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests-common has a workspace parent")
        .join("target");
    launcher_candidates(&target).into_iter().find(|p| p.is_file())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;

    use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
    use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};

    use super::{image_dir, launcher_skip_message, locate_microvm_run, probe_skip_message};

    /// The kernel + rootfs pair for `rootfs` (a bare filename such as
    /// `"web-fetch.ext4"`) inside [`image_dir`].
    pub fn firecracker_image_for(rootfs: &str) -> FirecrackerImage {
        let dir = std::path::PathBuf::from(image_dir());
        FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join(rootfs) }
    }

    /// Returns `true` if this host cannot boot `rootfs`, after printing a
    /// `[SKIP]` line saying which prerequisite is missing. Callers
    /// `return` immediately.
    ///
    /// Two gates, in the order an operator can act on them:
    ///
    /// 1. the Firecracker probe (`/dev/kvm`, `/dev/vhost-vsock`, and the
    ///    rootfs + kernel actually present), and
    /// 2. the VMM launcher being built.
    ///
    /// On success it prepends the launcher's directory to `PATH`,
    /// because the backend spawns it by bare name. That is a
    /// process-global mutation, but each integration-test binary is its
    /// own process and the `Once` makes repeated calls idempotent.
    /// Hoisting these 15 copies into one shared `Once` is strictly
    /// better than 15 independent ones.
    pub fn skip_if_no_microvm(rootfs: &str) -> bool {
        if let Err(e) = LinuxFirecracker::probe(&firecracker_image_for(rootfs)) {
            eprint!("{}", probe_skip_message(rootfs, &e.to_string()));
            return true;
        }
        match locate_microvm_run() {
            Some(bin) => {
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
                false
            }
            None => {
                eprint!("{}", launcher_skip_message());
                true
            }
        }
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
mod tests {
    use super::*;

    /// The repository root, derived from this crate's manifest dir.
    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tests-common has a workspace parent")
            .to_path_buf()
    }

    /// Every hint must name a script that actually exists, so a rename
    /// or a move breaks this test rather than silently sending an
    /// operator to a nonexistent path. This is the pin that lets the
    /// table stay hand-written instead of derived.
    #[test]
    fn every_build_script_exists() {
        let root = repo_root();
        for (rootfs, script) in ROOTFS_BUILD_SCRIPTS {
            let path = root.join(script);
            assert!(path.is_file(), "build script for {rootfs} is missing: {}", path.display());
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

    /// The regression this module was filed for: the launcher is probed
    /// release-first, so every operator-facing hint must say `--release`.
    /// Two of the 15 original copies said plain `cargo build -p …`,
    /// which rebuilds debug and leaves a stale release binary running.
    #[test]
    fn release_is_probed_before_debug() {
        let candidates = launcher_candidates(Path::new("/ws/target"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/ws/target/release").join(LAUNCHER_BIN),
                PathBuf::from("/ws/target/debug").join(LAUNCHER_BIN),
            ]
        );
    }

    #[test]
    fn launcher_hint_says_release() {
        let msg = launcher_skip_message();
        assert!(msg.contains("--release"), "hint must say --release, got: {msg}");
        assert!(msg.contains("[SKIP]"), "must be greppable as a skip: {msg}");
    }

    #[test]
    fn probe_message_names_the_rootfs_and_its_build_script() {
        let msg = probe_skip_message("web-fetch.ext4", "no /dev/kvm");
        assert!(msg.contains("web-fetch.ext4"), "must name the image: {msg}");
        assert!(msg.contains("no /dev/kvm"), "must carry the cause: {msg}");
        assert!(msg.contains("build-web-fetch-rootfs.sh"), "must point at the builder: {msg}");
    }

    #[test]
    fn probe_message_omits_the_build_hint_for_an_unknown_rootfs() {
        let msg = probe_skip_message("mystery.ext4", "boom");
        assert!(msg.contains("mystery.ext4"));
        assert!(!msg.contains("build the rootfs with"), "must not invent a script: {msg}");
    }
}
