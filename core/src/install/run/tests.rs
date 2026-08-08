//! Staging-path tests for the installer's file-replacement helpers.
//!
//! `copy_exec` and `symlink_replace` both publish their result by staging
//! next to the destination and renaming over it. The staging path must be
//! a function of the **writer**, not of the destination: a
//! destination-derived name means two concurrent `kastellan-cli install`
//! runs pick the same temp path, and the loser's `rename` fails `ENOENT`
//! (the production-side twin of the supervisor bug fixed in the #509
//! review — see `kastellan_supervisor::atomic_write`).
//!
//! Uniqueness makes cleanup mandatory in turn: a deterministic name meant
//! a retry overwrote the previous attempt's leftover, whereas a unique one
//! leaves one more file per failed write — and these land in the
//! operator's `~/.local/bin`, where the litter is executable.

use super::*;
use kastellan_tests_common::unique_temp_root;

/// `unique_temp_root` only *names* a path; create it.
fn test_root(label: &str) -> PathBuf {
    let dir = unique_temp_root(label);
    fs::create_dir_all(&dir).expect("create test root");
    dir
}

/// Names in `dir` that mark an in-flight staged write.
fn staging_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp-install."))
        .collect()
}

#[test]
fn staging_path_is_unique_per_call_for_one_destination() {
    let dest = Path::new("/opt/bin/kastellan-cli");
    let a = staging_path(dest);
    let b = staging_path(dest);
    assert_ne!(a, b, "two writers of one destination must not share a staging path");
    // Both stay beside the destination: the rename must not cross a
    // filesystem boundary, or it stops being atomic.
    assert_eq!(a.parent(), Some(Path::new("/opt/bin")));
    assert_eq!(b.parent(), Some(Path::new("/opt/bin")));
}

#[test]
fn staging_path_keeps_the_whole_destination_file_name() {
    // The former `with_extension("tmp-install")` REPLACED the final
    // `.`-component, so two differently-suffixed destinations sharing a
    // stem collapsed onto one staging path. Binaries are extensionless
    // today, but the invariant should not depend on that.
    let plain = staging_path(Path::new("/opt/bin/kastellan-cli"));
    let dotted = staging_path(Path::new("/opt/bin/kastellan-cli.sh"));
    let plain_name = plain.file_name().unwrap().to_string_lossy().into_owned();
    let dotted_name = dotted.file_name().unwrap().to_string_lossy().into_owned();

    assert!(plain_name.starts_with("kastellan-cli.tmp-install."), "{plain_name}");
    assert!(dotted_name.starts_with("kastellan-cli.sh.tmp-install."), "{dotted_name}");
}

#[test]
fn failed_copy_exec_removes_its_staging_file() {
    // Force the rename to fail by parking a directory where the binary
    // belongs — rename(2) refuses to replace a directory with a file.
    let dir = test_root("cp-fail");
    let src = dir.join("src-bin");
    fs::write(&src, b"#!/bin/sh\ntrue\n").expect("write src");
    let dest = dir.join("kastellan-cli");
    fs::create_dir_all(&dest).expect("blocking dir");

    copy_exec(&src, &dest).expect_err("rename over a directory must fail");
    assert!(
        staging_files(&dir).is_empty(),
        "failed copy left its staging file behind: {:?}",
        staging_files(&dir)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn failed_symlink_replace_removes_its_staging_link() {
    let dir = test_root("ln-fail");
    let target = dir.join("real-bin");
    fs::write(&target, b"#!/bin/sh\ntrue\n").expect("write target");
    let link = dir.join("kastellan-cli");
    fs::create_dir_all(&link).expect("blocking dir");

    symlink_replace(&target, &link).expect_err("rename over a directory must fail");
    assert!(
        staging_files(&dir).is_empty(),
        "failed symlink replace left its staging link behind: {:?}",
        staging_files(&dir)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_destructive_rewrite_backs_the_old_file_up() {
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("kastellan.env");
    fs::write(&env, "KASTELLAN_MAIL_ENDPOINT=https://h:8443\n").unwrap();

    preserve_and_report_env(&env, "KASTELLAN_DATA_DIR=/d\n").unwrap();

    let bak = dir.path().join("kastellan.env.bak");
    assert_eq!(
        fs::read_to_string(&bak).unwrap(),
        "KASTELLAN_MAIL_ENDPOINT=https://h:8443\n",
        "the backup is where the operator reads the values the warning omits"
    );
}

#[test]
fn a_non_destructive_rewrite_writes_no_backup() {
    // Gating on a non-empty diff is what stops a later clean install from
    // clobbering the one backup that mattered.
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("kastellan.env");
    fs::write(&env, "A=1\n").unwrap();

    preserve_and_report_env(&env, "A=1\nB=2\n").unwrap();

    assert!(!dir.path().join("kastellan.env.bak").exists());
}

#[test]
fn a_first_install_has_nothing_to_preserve() {
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("kastellan.env");
    preserve_and_report_env(&env, "A=1\n").expect("a missing env file is not an error");
    assert!(!dir.path().join("kastellan.env.bak").exists());
}

#[test]
fn the_warning_names_every_dropped_and_changed_key() {
    let diff = EnvDiff {
        lost: vec!["KASTELLAN_MAIL_ENDPOINT".into(), "KASTELLAN_MAIL_TOKEN_FILE".into()],
        changed: vec!["KASTELLAN_LLM_LOCAL_MODEL".into()],
    };
    let msg = render_drop_warning(
        Path::new("/h/.config/kastellan/kastellan.env"),
        Path::new("/h/.config/kastellan/kastellan.env.bak"),
        Path::new("/h/.config/kastellan/kastellan.env.local"),
        &diff,
    );
    // Every key must be named -- these are exactly the keys whose silent loss
    // cost the deployed agent its mail capability for two days.
    assert!(msg.contains("KASTELLAN_MAIL_ENDPOINT"), "{msg}");
    assert!(msg.contains("KASTELLAN_MAIL_TOKEN_FILE"), "{msg}");
    assert!(msg.contains("KASTELLAN_LLM_LOCAL_MODEL"), "{msg}");
}

#[test]
fn the_warning_points_at_the_backup_and_the_overlay() {
    // Without both pointers the operator is told something was lost and not
    // where to recover it from or how to stop it recurring.
    let diff = EnvDiff { lost: vec!["A".into()], changed: vec![] };
    let msg = render_drop_warning(
        Path::new("/h/kastellan.env"),
        Path::new("/h/kastellan.env.bak"),
        Path::new("/h/kastellan.env.local"),
        &diff,
    );
    assert!(msg.contains("kastellan.env.bak"), "{msg}");
    assert!(msg.contains("kastellan.env.local"), "{msg}");
}

#[test]
fn a_changed_only_diff_still_produces_a_warning() {
    // Pins the `changed` loop specifically: dropping it leaves `lost`-only
    // diffs working, so a lost-key test alone would not catch its removal.
    let diff = EnvDiff { lost: vec![], changed: vec!["KASTELLAN_LLM_LOCAL_MODEL".into()] };
    let msg = render_drop_warning(
        Path::new("/h/kastellan.env"),
        Path::new("/h/kastellan.env.bak"),
        Path::new("/h/kastellan.env.local"),
        &diff,
    );
    assert!(msg.contains("KASTELLAN_LLM_LOCAL_MODEL"), "{msg}");
}
