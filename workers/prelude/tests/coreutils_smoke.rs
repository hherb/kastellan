//! `BASE_ALLOW` coverage audit for common worker binaries (issue #5).
//!
//! Background: the seccomp `BASE_ALLOW` set was derived empirically from
//! `strace -fc` of `shell-exec` running `echo`. Echo doesn't read or
//! write files, so the original list had a silent gap for any binary
//! that does. It bit us when `cp` died with SIGSYS on first read; we
//! had to retro-add `copy_file_range`, `sendfile`, `fadvise64`. The
//! discovery method (a real worker dies) is unpleasant, and the next
//! gap will be found the same way unless we audit ahead of time.
//!
//! ## What this test does
//!
//! For each common coreutil — `cp`, `cat`, `mkdir`, …, `/bin/sh` — spawn
//! `hhagent-lockdown-probe exec-after-lockdown <binary> [args]`. The
//! probe locks down (seccomp `strict` + Landlock with the scratch dir
//! writable), then `execve`s into the coreutil. The seccomp filter
//! survives `execve` (`PR_SET_NO_NEW_PRIVS` was set), so any
//! non-allow-listed syscall the binary hits kills it with SIGSYS.
//!
//! A SIGSYS exit is a `BASE_ALLOW` gap — the test fails loudly with the
//! binary name so a future maintainer adding `python-exec` or similar
//! discovers the gap here, deterministically, instead of in a SIGSYS
//! crash log at runtime. A clean exit (or any non-SIGSYS error code)
//! proves the binary's syscall set is covered.
//!
//! ## Skip pattern
//!
//! Skips on hosts where:
//! * The kernel can't create an unprivileged user namespace (matches
//!   the pattern in `landlock_smoke.rs`).
//! * A given binary isn't present on the host (some minimal images
//!   ship without `tar` or `gzip`). Skipped binaries print a `[SKIP]`
//!   line via `eprintln!` so a `--nocapture` run shows which binaries
//!   the audit covered.
//!
//! Each test is isolated to its own scratch dir under
//! `/tmp/hhagent_prelude_coreutils_smoke/<test-name>/` so parallel
//! runs don't collide.

#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PROBE: &str = env!("CARGO_BIN_EXE_hhagent-lockdown-probe");

/// SIGSYS = the signal seccomp's `KillProcess` action delivers. If the
/// child exited with this signal, the coreutil hit a syscall that
/// isn't in `BASE_ALLOW`.
const SIGSYS: i32 = libc::SIGSYS;

const SCRATCH_ROOT: &str = "/tmp/hhagent_prelude_coreutils_smoke";

/// Per-test scratch dir. Created fresh (recursively removed first so a
/// crashed prior run doesn't leak state); the dir is the only path
/// Landlock will allow writes to.
fn prepare_scratch(test_name: &str) -> PathBuf {
    let p = PathBuf::from(SCRATCH_ROOT).join(test_name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create scratch dir");
    p
}

/// Run `binary` with `args` after locking down to seccomp `strict` +
/// Landlock with `rw_dir` writable. Returns the child's `Output` —
/// callers assert on signal/code.
fn run_under_lockdown(binary: &str, args: &[&str], rw_dir: &Path) -> Output {
    // Landlock RW env is a JSON array of absolute paths. The scratch
    // dir is the only writable path; everything else is read-only or
    // denied.
    let rw_json = serde_json::to_string(&[rw_dir]).expect("serialize rw_dir");
    let mut cmd_args: Vec<&str> = vec!["exec-after-lockdown", binary];
    cmd_args.extend_from_slice(args);
    Command::new(PROBE)
        .args(&cmd_args)
        // Clear env so the test isn't sensitive to developer-shell
        // leakage. `PATH` is set explicitly so children that resolve
        // their helpers via $PATH (rare for coreutils, but sed/awk
        // sometimes spawn subprocesses) can find them.
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HHAGENT_SECCOMP_PROFILE", "strict")
        .env("HHAGENT_LANDLOCK_RW", &rw_json)
        .output()
        .expect("failed to spawn lockdown-probe")
}

/// True iff the binary exists at the given absolute path. Skips
/// gracefully when a coreutil isn't installed (minimal containers).
fn binary_present(path: &str) -> bool {
    if Path::new(path).exists() {
        true
    } else {
        eprintln!("\n[SKIP] {path} not present on host\n");
        false
    }
}

/// Probe whether `lock_down()` itself works on this host. The Linux
/// integration tests skip when bwrap/userns/landlock won't load; we
/// detect the same way — `LOCKDOWN_REPORT: Linux { ... Installed }`
/// on stderr means the filter is enforcing.
fn lockdown_enforces() -> bool {
    let probe_out = Command::new(PROBE)
        .args(["exec-after-lockdown", "/bin/true"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HHAGENT_SECCOMP_PROFILE", "strict")
        .env(
            "HHAGENT_LANDLOCK_RW",
            serde_json::to_string(&[SCRATCH_ROOT]).unwrap(),
        )
        .output()
        .expect("spawn probe to check enforcement");
    let stderr = String::from_utf8_lossy(&probe_out.stderr);
    if !stderr.contains("Installed") {
        eprintln!(
            "\n[SKIP] seccomp filter not Installed; report:\n{stderr}\n"
        );
        return false;
    }
    true
}

/// Assert the child exited cleanly (no SIGSYS). Exit code itself can
/// be 0 or non-zero — coreutils can legitimately fail (e.g.,
/// permission-denied from Landlock) without that being a BASE_ALLOW
/// gap. SIGSYS specifically is the gap signal.
fn assert_no_sigsys(binary: &str, out: &Output) {
    if let Some(sig) = out.status.signal() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_ne!(
            sig, SIGSYS,
            "BASE_ALLOW gap: {binary} was killed by SIGSYS (signal {sig}). \
             A non-allow-listed syscall fired. Inspect with: \n  \
             strace -fc {binary} <args>\n\
             stderr: {stderr}\nstdout: {stdout}"
        );
    }
}

// ─── per-binary smoke tests ─────────────────────────────────────────
//
// Each test: scratch-dir setup, fixture file(s) if needed, run the
// command under lockdown, assert no SIGSYS. Failure mode pinpoints the
// missing syscall(s) — fix by adding to `BASE_ALLOW` with a one-line
// justification (no capability beyond what `openat` already grants),
// adding to a narrower profile (e.g. `Profile::CoreutilClient`), or
// refusing and requiring the worker to do the operation differently.

#[test]
fn cat_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/cat";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("cat");
    let src = dir.join("input.txt");
    std::fs::write(&src, "hello\nworld\n").expect("write fixture");
    let out = run_under_lockdown(bin, &[src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn cp_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/cp";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("cp");
    let src = dir.join("src.txt");
    let dst = dir.join("dst.txt");
    std::fs::write(&src, "payload").expect("write fixture");
    let out = run_under_lockdown(
        bin,
        &[src.to_str().unwrap(), dst.to_str().unwrap()],
        &dir,
    );
    assert_no_sigsys(bin, &out);
}

#[test]
fn ls_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/ls";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("ls");
    std::fs::write(dir.join("a"), "").expect("write fixture a");
    std::fs::write(dir.join("b"), "").expect("write fixture b");
    let out = run_under_lockdown(bin, &[dir.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn mkdir_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/mkdir";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("mkdir");
    let target = dir.join("newdir");
    let out = run_under_lockdown(bin, &[target.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn touch_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/touch";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("touch");
    let target = dir.join("newfile");
    let out = run_under_lockdown(bin, &[target.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn mv_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/mv";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("mv");
    let src = dir.join("src.txt");
    let dst = dir.join("dst.txt");
    std::fs::write(&src, "payload").expect("write fixture");
    let out = run_under_lockdown(
        bin,
        &[src.to_str().unwrap(), dst.to_str().unwrap()],
        &dir,
    );
    assert_no_sigsys(bin, &out);
}

#[test]
fn rm_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/rm";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("rm");
    let target = dir.join("doomed.txt");
    std::fs::write(&target, "delete me").expect("write fixture");
    let out = run_under_lockdown(bin, &[target.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn grep_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/grep";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("grep");
    let src = dir.join("haystack.txt");
    std::fs::write(&src, "alpha\nbeta\ngamma\n").expect("write fixture");
    let out = run_under_lockdown(bin, &["beta", src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn sed_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/sed";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("sed");
    let src = dir.join("input.txt");
    std::fs::write(&src, "foo bar\n").expect("write fixture");
    let out = run_under_lockdown(bin, &["s/foo/baz/", src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn awk_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    // awk is commonly /usr/bin/awk (symlink to mawk or gawk).
    let bin = "/usr/bin/awk";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("awk");
    let src = dir.join("input.txt");
    std::fs::write(&src, "one two three\n").expect("write fixture");
    let out = run_under_lockdown(bin, &["{print $2}", src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn sort_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/sort";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("sort");
    let src = dir.join("unsorted.txt");
    std::fs::write(&src, "gamma\nalpha\nbeta\n").expect("write fixture");
    let out = run_under_lockdown(bin, &[src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn uniq_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/uniq";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("uniq");
    let src = dir.join("dupes.txt");
    std::fs::write(&src, "a\na\nb\nb\nc\n").expect("write fixture");
    let out = run_under_lockdown(bin, &[src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn head_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/head";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("head");
    let src = dir.join("input.txt");
    std::fs::write(&src, "1\n2\n3\n4\n5\n").expect("write fixture");
    let out = run_under_lockdown(bin, &["-n", "2", src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn tail_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/tail";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("tail");
    let src = dir.join("input.txt");
    std::fs::write(&src, "1\n2\n3\n4\n5\n").expect("write fixture");
    let out = run_under_lockdown(bin, &["-n", "2", src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn wc_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/wc";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("wc");
    let src = dir.join("input.txt");
    std::fs::write(&src, "alpha\nbeta\ngamma\n").expect("write fixture");
    let out = run_under_lockdown(bin, &["-l", src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn find_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/find";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("find");
    std::fs::write(dir.join("a.txt"), "").expect("write fixture");
    let out = run_under_lockdown(
        bin,
        &[dir.to_str().unwrap(), "-maxdepth", "1"],
        &dir,
    );
    assert_no_sigsys(bin, &out);
}

#[test]
fn tar_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/tar";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("tar");
    let src = dir.join("payload.txt");
    let tarball = dir.join("out.tar");
    std::fs::write(&src, "payload").expect("write fixture");
    // `--numeric-owner` skips uid/gid → name lookups via NSS, which
    // would open a socket() to nscd and trip the BSD-socket restriction
    // under `Profile::Strict`. (Tar's NSS lookup is incidental — for
    // worker use cases, numeric ownership is the right policy anyway:
    // we don't want random uids leaking from the host into archives.)
    // Without `--numeric-owner` tar would SIGSYS on `socket()`, which
    // is NOT a BASE_ALLOW gap — it's the intentional Strict vs
    // NetClient boundary.
    //
    // `-C dir` so the tarball stores `payload.txt`, not the absolute path.
    let out = run_under_lockdown(
        bin,
        &[
            "--numeric-owner",
            "-cf",
            tarball.to_str().unwrap(),
            "-C",
            dir.to_str().unwrap(),
            "payload.txt",
        ],
        &dir,
    );
    assert_no_sigsys(bin, &out);
}

#[test]
fn gzip_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    let bin = "/usr/bin/gzip";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("gzip");
    let src = dir.join("payload.txt");
    std::fs::write(&src, "compressible content content content\n").expect("write fixture");
    let out = run_under_lockdown(bin, &[src.to_str().unwrap()], &dir);
    assert_no_sigsys(bin, &out);
}

#[test]
fn sh_true_survives_strict() {
    if !lockdown_enforces() {
        return;
    }
    // POSIX shell. Useful if a future worker augmentation runs
    // `sh -c "single-command"`.
    let bin = "/bin/sh";
    if !binary_present(bin) {
        return;
    }
    let dir = prepare_scratch("sh_true");
    let out = run_under_lockdown(bin, &["-c", "true"], &dir);
    assert_no_sigsys(bin, &out);
}
