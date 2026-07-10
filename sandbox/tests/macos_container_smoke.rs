//! End-to-end tests for the macOS Apple `container` micro-VM backend.
//! These actually invoke `/opt/homebrew/bin/container` and spawn a real
//! Linux micro-VM per call, so they only run on macOS hosts where the
//! prerequisites are met:
//!   - `container` binary on `$PATH` (install: `brew install container`)
//!   - Container system service running
//!     (`container system start --enable-kernel-install`)
//!   - The `alpine:3.20` image already pulled
//!     (`container image pull alpine:3.20`)
//!
//! Tests that can't run are printed as `[SKIP]` lines on stderr via
//! `eprintln!`. Per project convention, green output with `[SKIP]` lines
//! means the test was skipped, NOT that the container actually contained
//! anything — always check `cargo test -- --nocapture` if you suspect a
//! false green.
//!
//! Parallel to `macos_smoke.rs` for the Seatbelt backend.
//!
//! Scope note: `SandboxPolicy::cpu_ms` enforcement is deliberately not
//! exercised here. It flows via `KASTELLAN_CPU_MS` + `workers/prelude::rlimit`
//! inside the container; the smoke tests use opaque commands (`/bin/echo`,
//! `/bin/cat`, `/bin/sh`) that don't run the worker prelude. End-to-end
//! validation through a real worker lands in Slice 2.5 alongside the
//! `gliner-relex` Containerfile.

#![cfg(target_os = "macos")]

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use kastellan_sandbox::macos_container::{MacosContainer, DEFAULT_IMAGE};
use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

/// Skip the test if Apple `container` is unavailable on this host. Prints
/// to stderr via `eprintln!` so `cargo test -- --nocapture` shows the
/// reason. Same pattern as [`skip_if_no_seatbelt`] in `macos_smoke.rs`.
fn skip_if_no_container() -> bool {
    if let Err(e) = MacosContainer::probe() {
        eprintln!("\n[SKIP] Apple `container` probe failed: {e}\n");
        return true;
    }
    if !alpine_image_is_cached() {
        eprintln!(
            "\n[SKIP] {DEFAULT_IMAGE} not in `container image list` — \
             run `container image pull {DEFAULT_IMAGE}` to enable container smoke tests\n"
        );
        return true;
    }
    false
}

/// Returns true iff `container image list` shows the smoke-test image as
/// already pulled. Tests skip on absence rather than triggering a multi-GB
/// pull-on-CI surprise.
fn alpine_image_is_cached() -> bool {
    let output = match Command::new("container").args(["image", "list"]).output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Output is a fixed-width table with `alpine 3.20 <digest>` as one
    // line per image. Substring match is enough for the smoke-test
    // pre-check (we don't need to parse the table strictly).
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("alpine") && trimmed.contains("3.20")
    })
}

fn strict_policy() -> SandboxPolicy {
    SandboxPolicy {
        cpu_ms: 30_000,
        ..SandboxPolicy::default()
    }
}

fn read_to_string(handle: &mut Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(h) = handle.as_mut() {
        let _ = h.read_to_string(&mut s);
    }
    s
}

/// Process-local + clock-based tempdir naming so concurrent test cases
/// can't collide. Mirrors the `audit_tail::tests::tempdir` shape applied
/// in PR #102 to close the macOS-microsecond-clock collision found in
/// Issue #101.
fn tempdir(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nanos}-{n}"));
    std::fs::create_dir_all(&path).expect("create tempdir");
    path
}

#[test]
fn scaffold_compiles_and_skip_helper_runs() {
    // This test exists so we verify the scaffolding builds and the skip
    // helper executes without panicking on hosts without container.
    let _ = skip_if_no_container();
    let _ = strict_policy();
    let _: fn(&mut Option<std::process::ChildStdout>) -> String = read_to_string;
}

/// Headline integration: `container run` actually spawns and stdout
/// round-trips cleanly. Pin proves the full
/// `MacosContainer::spawn_under_policy` → argv → process boundary works
/// end-to-end, not just at the unit level.
#[test]
fn echo_runs_inside_container() {
    if skip_if_no_container() {
        return;
    }
    let backend = MacosContainer::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/echo", &["hello-from-jail"])
        .expect("container should spawn echo");
    let status = child.wait().expect("wait");
    let stderr = read_to_string(&mut child.stderr);
    let stdout = read_to_string(&mut child.stdout);
    assert!(
        status.success(),
        "echo exited non-zero: {status:?}; stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(stdout.trim_end(), "hello-from-jail");
}

/// Host `/Users` must be invisible inside the container (no implicit
/// bind-mount). If a future container release changed the default and
/// started mounting the host's home tree, this test would fire.
#[test]
fn host_users_dir_is_invisible_when_not_mounted() {
    if skip_if_no_container() {
        return;
    }
    let host_user = std::env::var("USER").ok();

    let backend = MacosContainer::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/ls", &["/Users"])
        .expect("container should spawn ls");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);

    // /Users does not exist inside an alpine container — ls must fail.
    assert!(
        !status.success(),
        "ls /Users should fail inside container (no implicit host mount); \
         status={status:?} stdout={stdout:?} stderr={stderr:?}"
    );

    // Even if a future Apple change accidentally exposed /Users, the host
    // username must never leak. Belt-and-braces.
    if let Some(user) = host_user {
        assert!(
            !stdout.contains(&user),
            "container leaked host username {user:?}; stdout={stdout:?} stderr={stderr:?}"
        );
    }
}

/// `fs_read` bind-mount: a file the host wrote must be readable inside
/// the container at the same path. Validates the `--mount type=bind,...,readonly`
/// flag wiring.
#[test]
fn fs_read_bind_mount_makes_host_file_visible_to_container() {
    if skip_if_no_container() {
        return;
    }
    let dir = tempdir("kastellan-container-smoke-fsread");
    let file = dir.join("greeting.txt");
    std::fs::write(&file, "hello-from-host").expect("write fixture");
    // World-readable so the container's `nobody` user can read it (the
    // strict profile forces --user nobody).
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
        .expect("chmod greeting");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .expect("chmod dir");

    let mut policy = strict_policy();
    policy.fs_read.push(dir.clone());

    let backend = MacosContainer::new();
    let target = file.to_string_lossy().into_owned();
    let mut child = backend
        .spawn_under_policy(&policy, "/bin/cat", &[&target])
        .expect("container should spawn cat");
    let status = child.wait().expect("wait");
    let stderr = read_to_string(&mut child.stderr);
    let stdout = read_to_string(&mut child.stdout);
    assert!(
        status.success(),
        "cat of bind-mounted file failed: {status:?} stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(stdout.trim_end(), "hello-from-host");

    // Clean up the tempdir on success (Drop guards would be nicer; for a
    // few smoke tests this is fine).
    let _ = std::fs::remove_dir_all(&dir);
}

/// `fs_read` bind-mount is read-only — writing to a mounted path must
/// fail with the container's `Read-only file system` errno. Mirrors the
/// bwrap `--ro-bind` posture.
#[test]
fn fs_read_bind_mount_rejects_writes() {
    if skip_if_no_container() {
        return;
    }
    let dir = tempdir("kastellan-container-smoke-fsread-rw");
    let file = dir.join("ro-target.txt");
    std::fs::write(&file, "initial").expect("write fixture");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666))
        .expect("chmod target");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
        .expect("chmod dir");

    let mut policy = strict_policy();
    policy.fs_read.push(dir.clone());

    let backend = MacosContainer::new();
    // Use sh to attempt a write into the mounted file; success means the
    // sh exited 0 (write succeeded), which is the BUG case.
    let target = file.to_string_lossy().into_owned();
    let shell_cmd = format!("echo overwritten > {target}");
    let mut child = backend
        .spawn_under_policy(&policy, "/bin/sh", &["-c", &shell_cmd])
        .expect("container should spawn sh");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    assert!(
        !status.success(),
        "write to readonly bind-mount should fail; status={status:?} \
         stdout={stdout:?} stderr={stderr:?}"
    );
    // The failure must be EROFS (`Read-only file system`), not a
    // permission error from the strict-profile `--user nobody` running
    // against an unfavourable host mode. The file is 0o666 + parent
    // 0o777, so `nobody` (UID 65534) has world-write — if this assertion
    // ever flips to permission-denied, the bind-mount is probably no
    // longer being marked readonly. Lowercased so we tolerate `Read-only`
    // vs `read-only` formatting drift across container/sh versions.
    let combined = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        combined.contains("read-only"),
        "expected EROFS-style failure (`Read-only file system`); \
         stdout={stdout:?} stderr={stderr:?}"
    );
    // Verify the host file content is unchanged (defense in depth — even
    // if the container reported failure, the host file must still be its
    // original content).
    let host_content = std::fs::read_to_string(&file).unwrap_or_default();
    assert_eq!(
        host_content.trim_end(),
        "initial",
        "readonly bind-mount let a write through to the host file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--read-only` flag from `Profile::WorkerStrict` makes the container's
/// root FS read-only — writes outside the explicit `--tmpfs /tmp` scratch
/// must fail. Validates the strict profile's `--read-only` plumbing.
#[test]
fn strict_profile_makes_root_fs_readonly() {
    if skip_if_no_container() {
        return;
    }
    let backend = MacosContainer::new();
    let mut child = backend
        .spawn_under_policy(
            &strict_policy(),
            "/bin/sh",
            &["-c", "echo x > /usr/bin/marker 2>&1; echo exit=$?"],
        )
        .expect("container should spawn sh");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    // The sh itself exits 0 (we wrapped with `echo exit=$?`); the
    // redirected-write inside should fail. Assertions:
    //   (a) the sh ran (status 0, since the wrapper succeeded)
    //   (b) stdout contains "exit=" with a non-zero value
    //   (c) stdout or stderr contains a "Read-only file system" hint —
    //       pins that the failure is EROFS (i.e. `--read-only` was
    //       actually applied) rather than e.g. `Permission denied` from
    //       `--user nobody` (which would also fail but for the wrong
    //       reason)
    assert!(
        status.success(),
        "wrapper sh should exit 0; status={status:?} stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("exit=") && !stdout.contains("exit=0"),
        "expected non-zero exit code from write; stdout={stdout:?} stderr={stderr:?}"
    );
    let combined = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        combined.contains("read-only"),
        "expected `Read-only file system` hint in output; \
         stdout={stdout:?} stderr={stderr:?}"
    );
}

/// `--tmpfs /tmp` from the strict profile gives a writable scratch atop
/// the otherwise-read-only root. Pin the positive case so a future
/// refactor that drops the tmpfs flag is loud.
#[test]
fn strict_profile_allows_writes_to_tmp_scratch() {
    if skip_if_no_container() {
        return;
    }
    let backend = MacosContainer::new();
    let mut child = backend
        .spawn_under_policy(
            &strict_policy(),
            "/bin/sh",
            &["-c", "echo greeting > /tmp/marker && cat /tmp/marker"],
        )
        .expect("container should spawn sh");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    assert!(
        status.success(),
        "write+read in /tmp scratch failed; status={status:?} \
         stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(stdout.trim_end(), "greeting");
}

/// Slice 2.5 (Issue #107 follow-up): `--init` is always-on in
/// build_container_argv. This smoke verifies that the added flag
/// doesn't break Apple `container`'s short-lived run envelope. If
/// `--init` is rejected by an older `container` build, this test
/// fails loudly instead of letting the broken argv ship.
#[test]
fn macos_container_argv_with_init_runs_alpine_cleanly() {
    if skip_if_no_container() {
        return;
    }
    let backend = MacosContainer::new();  // default image = alpine:3.20
    let policy = SandboxPolicy {
        // Minimal policy: just enough so --init has something to wrap
        // around and the spawn returns quickly.
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 256,  // above container's 200 MiB floor; no clamp warn
        profile: Profile::WorkerStrict,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![],
        proxy_uds: None,
        embed_broker_uds: None,
        persistent_store: None,
    };
    let mut child = match backend.spawn_under_policy(
        &policy,
        "/bin/sh",
        &["-c", "echo init-ok"],
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\n[SKIP] alpine:3.20 image likely missing: {e}\n");
            return;
        }
    };
    let stderr = read_to_string(&mut child.stderr);
    let status = child.wait().expect("wait on container run");
    assert!(
        status.success(),
        "container run [policy flags] --init alpine:3.20 /bin/sh -c 'echo init-ok' must exit 0; \
         got {status:?}; stderr={stderr:?}"
    );
}

/// `MacosContainer::probe_image` smoke test (issue #120) — verifies the
/// real-spawn path against `container image inspect <tag>`.
///
/// Strategy: probe the same image the other smoke tests rely on
/// (`alpine:3.20`, cached on the host as a pre-condition of the test
/// suite). Probing should return `Ok(())`. Then probe a deliberately
/// non-existent tag and assert the helper returns `Err` with an
/// operator-actionable message mentioning the missing tag.
///
/// Skip-as-pass when Apple `container` is unavailable OR when the
/// expected reference image (`alpine:3.20`) is not cached on the host.
#[test]
fn probe_image_returns_ok_for_cached_image_and_err_for_missing_tag() {
    if skip_if_no_container() {
        return;
    }

    // Happy path: alpine:3.20 must be present (the suite skip-helper
    // already guaranteed it via `alpine_image_is_cached`).
    MacosContainer::probe_image(DEFAULT_IMAGE)
        .expect("probe_image(alpine:3.20) must succeed when the image is cached");

    // Missing path: a fresh nanos-suffixed tag we know we never built.
    // `container image inspect` exits non-zero on absence; helper maps
    // that to `Err(SandboxError::Backend(...))` carrying the missing tag.
    let bogus_tag = format!(
        "kastellan/definitely-not-built-{}:nonexistent",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let err = MacosContainer::probe_image(&bogus_tag)
        .expect_err("probe_image on a non-existent tag must error out");
    let msg = format!("{err}");
    assert!(
        msg.contains(&bogus_tag),
        "error message must surface the missing tag for operator triage; got: {msg}"
    );
    assert!(
        msg.contains("not present"),
        "error message must say the image is not present; got: {msg}"
    );
}
