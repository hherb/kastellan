//! Behavioural tests for `scripts/workers/microvm/rebuild-all-rootfs.sh`.
//!
//! The script is the one command every staleness message names, and until
//! the #680 review it was pinned only by "the file exists, is executable,
//! and mentions these paths". The two things it actually promises — attempt
//! every image, and exit non-zero if any failed — were untested.
//!
//! That gap has teeth. The script runs `set -uo pipefail` **without** `-e`
//! precisely so one failing image (browser-driver needs docker) does not stop
//! the other seven. A future tidy-up to `set -euo pipefail` would abort on
//! the first failure, never print the summary, and hand back a partial
//! rebuild reported as a stop — the exact stale-image outcome the script
//! exists to prevent. Nothing failed when that was tried.
//!
//! Each test runs the real script against a throwaway tree of stub build
//! scripts, so the assertions are about the script's own control flow and
//! never build a rootfs.

use std::path::{Path, PathBuf};
use std::process::Output;

use super::{repo_root, ROOTFS_IMAGES};

/// A throwaway workspace whose build scripts are stubs.
///
/// Removes itself on drop, including on a failing assertion — the tree is
/// eight tiny files, but the suite creates one per test and `/tmp` is
/// scrubbed unpredictably on both dev hosts.
struct StubTree {
    root: PathBuf,
}

impl Drop for StubTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl StubTree {
    /// Lay out the real script at its real relative path, with a stub at
    /// every path the registry names. `failing` stems exit 1.
    fn new(failing: &[&str]) -> Self {
        let root = crate::temp::unique_temp_root("rebuild-all");
        let dir = root.join("scripts/workers/microvm");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(
            repo_root().join(super::REBUILD_ALL_SCRIPT),
            dir.join("rebuild-all-rootfs.sh"),
        )
        .expect("copy the script under test");

        for entry in ROOTFS_IMAGES {
            let stem = entry.image.strip_suffix(".ext4").expect(".ext4");
            let path = root.join(entry.build_script);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            let code = if failing.contains(&stem) { 1 } else { 0 };
            std::fs::write(&path, format!("#!/usr/bin/env bash\necho built {stem}\nexit {code}\n"))
                .expect("write stub");
        }
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn script(&self) -> PathBuf {
        self.path().join(super::REBUILD_ALL_SCRIPT)
    }

    /// Run the script from a directory that is NOT the workspace root, so
    /// the tests also cover its `cd` to the root.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = std::process::Command::new("bash");
        cmd.arg(self.script()).args(args).current_dir(self.path().join("scripts"));
        // Bounded (#690): the stub tree makes this fast, and a budget means a
        // script that loops names itself instead of hanging the sweep.
        kastellan_sandbox::bounded_command::probe_output(
            &mut cmd,
            kastellan_sandbox::bounded_command::PROBE_BUDGET,
        )
        .unwrap_or_else(|e| panic!("the rebuild script did not answer: {e:?}"))
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The happy path: every registered image is attempted, and success is 0.
#[test]
fn a_clean_run_builds_every_registered_image_and_exits_zero() {
    let tree = StubTree::new(&[]);
    let out = tree.run(&[]);
    let stdout = stdout_of(&out);
    assert!(out.status.success(), "expected 0, got {:?}: {}", out.status, stderr_of(&out));
    for entry in ROOTFS_IMAGES {
        let stem = entry.image.strip_suffix(".ext4").expect(".ext4");
        assert!(stdout.contains(&format!("built {stem}")), "{stem} never ran: {stdout}");
    }
    assert!(stdout.contains("failed:  (none)"), "must report a clean summary: {stdout}");
}

/// The load-bearing promise. One image failing must NOT stop the others —
/// browser-driver needs docker, and an operator without it must still be
/// able to bring the other seven up to date.
#[test]
fn one_failing_image_does_not_stop_the_rest() {
    let tree = StubTree::new(&["browser-driver"]);
    let out = tree.run(&[]);
    let stdout = stdout_of(&out);
    for entry in ROOTFS_IMAGES {
        let stem = entry.image.strip_suffix(".ext4").expect(".ext4");
        if stem == "browser-driver" {
            continue;
        }
        assert!(
            stdout.contains(&format!("built {stem}")),
            "{stem} was skipped after an earlier failure — `set -e` crept in? {stdout}"
        );
    }
}

/// ...and the run must still FAIL. A partial rebuild reported as success
/// leaves exactly the stale image this script exists to prevent.
#[test]
fn any_failure_makes_the_whole_run_exit_non_zero() {
    let tree = StubTree::new(&["browser-driver"]);
    let out = tree.run(&[]);
    assert!(!out.status.success(), "a partial rebuild must not report success");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("browser-driver"), "must name what failed: {stderr}");
    assert!(stderr.contains("#667"), "must say why it matters: {stderr}");
}

/// A selector builds only what was asked for.
#[test]
fn a_selector_builds_only_the_named_images() {
    let tree = StubTree::new(&[]);
    let out = tree.run(&["kv-demo"]);
    let stdout = stdout_of(&out);
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout.contains("built kv-demo"), "{stdout}");
    assert!(!stdout.contains("built web-fetch"), "must not build what was not asked: {stdout}");
}

/// An unknown selector must be refused up front. Building nothing and
/// exiting 0 would be the same class of silent no-op the script exists to
/// end — and the operator would believe their images were rebuilt.
#[test]
fn an_unknown_selector_is_refused_before_anything_is_built() {
    let tree = StubTree::new(&[]);
    let out = tree.run(&["kv-demoo"]);
    assert_eq!(out.status.code(), Some(2), "unknown selectors get their own status");
    let stdout = stdout_of(&out);
    assert!(!stdout.contains("built "), "nothing may be built: {stdout}");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("Unknown image: kv-demoo"), "must name it: {stderr}");
    assert!(stderr.contains("kv-demo"), "must list what IS known: {stderr}");
}

/// A build script that has gone missing must be reported as a failure, not
/// silently skipped — a rename that outran this table would otherwise
/// shrink the rebuild to whatever still happened to exist.
#[test]
fn a_missing_build_script_is_a_failure_not_a_skip() {
    let tree = StubTree::new(&[]);
    let victim = tree.path().join(ROOTFS_IMAGES[0].build_script);
    std::fs::remove_file(&victim).expect("remove one stub");
    let out = tree.run(&[]);
    assert!(!out.status.success(), "a missing script must fail the run");
    assert!(stderr_of(&out).contains("MISSING build script"), "{}", stderr_of(&out));
}

/// The script must locate the workspace root from its own path, not from
/// the caller's cwd — every test above already runs it from `scripts/`, and
/// this pins the failure mode if that ever regresses.
#[test]
fn the_script_runs_from_the_workspace_root_whatever_the_cwd() {
    let tree = StubTree::new(&[]);
    let mut cmd = std::process::Command::new("bash");
    cmd.arg(tree.script()).current_dir(std::env::temp_dir());
    // Bounded (#690), as everywhere else in this module.
    let out = kastellan_sandbox::bounded_command::probe_output(
        &mut cmd,
        kastellan_sandbox::bounded_command::PROBE_BUDGET,
    )
    .unwrap_or_else(|e| panic!("the rebuild script did not answer: {e:?}"));
    assert!(out.status.success(), "must not depend on cwd: {}", stderr_of(&out));
}
