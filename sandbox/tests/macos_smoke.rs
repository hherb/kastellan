//! End-to-end tests for the macOS Seatbelt backend. These actually invoke
//! `/usr/bin/sandbox-exec`, so they only run on macOS.

#![cfg(target_os = "macos")]

use std::io::Read;
#[allow(unused_imports)]
use std::path::PathBuf;

#[allow(unused_imports)]
use kastellan_sandbox::{macos_seatbelt::MacosSeatbelt, SandboxBackend, SandboxPolicy};

/// Skip the test if Seatbelt is unavailable on this host. Prints to stderr
/// via `eprintln!` so `cargo test -- --nocapture` shows the skip line —
/// `[SKIP]` lines in green output mean tests skipped, not that Seatbelt
/// actually contained anything. Identical pattern to linux_smoke's
/// `skip_if_no_userns`.
fn skip_if_no_seatbelt() -> bool {
    match MacosSeatbelt::probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
            true
        }
    }
}

fn strict_policy() -> SandboxPolicy {
    SandboxPolicy {
        cpu_ms: 5_000,
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

#[test]
fn scaffold_compiles_and_skip_helper_runs() {
    // This test exists so we verify the scaffolding builds and the skip
    // helper executes without panicking. Real assertions land in Task 11+.
    let _ = skip_if_no_seatbelt();
    let _ = strict_policy();
    let _: fn(&mut Option<std::process::ChildStdout>) -> String = read_to_string;
}

#[test]
fn echo_runs_inside_sandbox() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/echo", &["hello-from-jail"])
        .expect("sandbox-exec should spawn echo");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "echo exited non-zero: {status:?}, stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert_eq!(stdout.trim_end(), "hello-from-jail");
}

#[test]
fn host_etc_master_passwd_is_invisible_when_not_in_policy() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    // /etc/master.passwd is the shadow file on macOS. /etc/passwd itself
    // is world-readable on macOS by design; master.passwd is the sensitive
    // analogue of Linux's /etc/passwd in this test's intent.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/cat", &["/etc/master.passwd"])
        .expect("sandbox-exec should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "cat /etc/master.passwd should fail inside sandbox; stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
}

#[test]
fn host_users_dir_is_invisible_when_not_in_policy() {
    if skip_if_no_seatbelt() {
        return;
    }
    // Capture the host's username up front so the assertion isn't hard-coded
    // to one developer machine. On a CI host without a $USER env var the
    // test still runs — we just lose the username-leak check and rely on the
    // exit-status assertion below.
    let host_user = std::env::var("USER").ok();

    let backend = MacosSeatbelt::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/ls", &["/Users"])
        .expect("sandbox-exec should spawn ls");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);

    // Primary assertion: ls /Users must fail under the strict profile, since
    // /Users is not in the allowlist. A successful listing here would mean
    // the deny-default posture has been broken.
    assert!(
        !status.success(),
        "ls /Users should be denied under strict profile; \
         status={status:?} stdout={stdout:?} stderr={stderr:?}"
    );

    // Secondary defence: even if a future broadening accidentally lets ls
    // succeed (e.g. by exposing /Users as a metadata-readable subpath), the
    // host user's name must not leak into stdout.
    if let Some(user) = host_user {
        assert!(
            !stdout.contains(&user),
            "sandbox leaked the host's username {user:?} via /Users! \
             stdout={stdout:?} stderr={stderr:?}"
        );
    }
}

#[test]
fn fs_read_path_is_visible_when_listed() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("/etc/hosts"));
    let mut child = backend
        .spawn_under_policy(&policy, "/bin/cat", &["/etc/hosts"])
        .expect("sandbox-exec should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "cat /etc/hosts should succeed when listed; stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert!(!stdout.is_empty(), "expected non-empty /etc/hosts content");
}

#[test]
fn relative_policy_paths_are_rejected() {
    let backend = MacosSeatbelt::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("relative/path"));
    let res = backend.spawn_under_policy(&policy, "/usr/bin/true", &[]);
    assert!(matches!(res, Err(kastellan_sandbox::SandboxError::Backend(_))));
}

#[test]
fn reading_dev_disk0_is_denied() {
    if skip_if_no_seatbelt() {
        return;
    }
    let backend = MacosSeatbelt::new();
    // /dev/disk0 is not in the explicit /dev allowlist, so the read must fail.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/bin/cat", &["/dev/disk0"])
        .expect("sandbox-exec should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "cat /dev/disk0 should be denied; stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
}

fn net_probe_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("net_probe")
}

fn sid_probe_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("sid_probe")
}

fn mach_probe_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("mach_probe")
}

/// Issue #1: the strict profile must NOT grant `mach-lookup`. Apple Events
/// broker (`com.apple.coreservices.appleevents`) is a non-essential Mach
/// service — none of our shipping workers (Rust binaries that don't link
/// libdispatch heavily) need it, and it's the canonical privilege-escalation
/// surface our threat model wants closed off (it's the back-end for
/// AppleScript-driven cross-app automation).
///
/// Verification: spawn `mach_probe` under the strict profile and assert it
/// exits non-zero. Outside the sandbox, mach_probe always exits 0 (verified
/// in the fixture's own doc comment); inside the strict profile, the
/// `bootstrap_look_up` call must be killed before it ever reaches launchd.
#[test]
fn worker_cannot_look_up_arbitrary_mach_services() {
    if skip_if_no_seatbelt() {
        return;
    }
    let probe = mach_probe_binary();
    if !probe.exists() {
        eprintln!(
            "[SKIP] mach_probe binary not built at {probe:?} — run `cargo build --workspace` first"
        );
        return;
    }
    let mut policy = strict_policy();
    policy.fs_read.push(probe.clone());

    let backend = MacosSeatbelt::new();
    let probe_str = probe.to_string_lossy().into_owned();
    let mut child = backend
        .spawn_under_policy(&policy, &probe_str, &[])
        .expect("sandbox-exec should spawn mach_probe");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    assert!(
        !status.success(),
        "mach_probe must NOT successfully look up com.apple.coreservices.appleevents \
         under the strict profile. status={status:?} stdout={stdout:?} stderr={stderr:?}; \
         this means the strict profile is granting `mach-lookup` and a worker can talk \
         to arbitrary registered launchd services. See issue #1."
    );
    // Ensure the failure was the expected kind (lookup denied, not exec failure).
    assert!(
        stderr.contains("bootstrap_look_up failed"),
        "mach_probe failed for an unexpected reason — expected `bootstrap_look_up failed: kr=...` \
         on stderr, got stdout={stdout:?} stderr={stderr:?}"
    );
}

/// Issue #2: every Seatbelt-launched worker must run in its own session,
/// not just its own process group. The Linux backend gets this for free
/// via `bwrap --new-session` (which calls `setsid`); on macOS we previously
/// used `Command::process_group(0)` (which calls `setpgid`), leaving the
/// worker attached to the parent's controlling terminal.
///
/// Verification: spawn `sid_probe` under the strict profile, parse
/// `<pid> <sid>` from stdout, and assert `sid == pid` (the worker is the
/// session leader of a fresh session). A simpler "sid != parent_sid"
/// check would also work, but the `sid == pid` invariant is stronger:
/// the only way to satisfy it is to have actually called `setsid()` in
/// the child, *not* to have inherited a different session because the
/// test binary itself was started detached.
#[test]
fn worker_runs_in_its_own_session() {
    if skip_if_no_seatbelt() {
        return;
    }
    let probe = sid_probe_binary();
    if !probe.exists() {
        eprintln!(
            "[SKIP] sid_probe binary not built at {probe:?} — run `cargo build --workspace` first"
        );
        return;
    }
    // The probe binary needs to be readable inside the sandbox.
    let mut policy = strict_policy();
    policy.fs_read.push(probe.clone());

    let backend = MacosSeatbelt::new();
    let probe_str = probe.to_string_lossy().into_owned();
    let mut child = backend
        .spawn_under_policy(&policy, &probe_str, &[])
        .expect("sandbox-exec should spawn sid_probe");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    assert!(
        status.success(),
        "sid_probe exited non-zero: {status:?}; stdout={stdout:?} stderr={stderr:?}"
    );

    let mut parts = stdout.split_whitespace();
    let pid: i32 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("expected `<pid> <sid>` on stdout, got {stdout:?}"));
    let sid: i32 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("expected `<pid> <sid>` on stdout, got {stdout:?}"));

    assert_eq!(
        sid, pid,
        "worker must be a session leader (sid == pid). Got pid={pid} sid={sid}; \
         this means setsid() was not called before exec — the worker is still \
         attached to the parent's session/controlling terminal. See issue #2."
    );

    // Belt-and-braces: the worker's session must also differ from the test
    // process's own session. Hard to fail given `sid == pid` already passed,
    // but documents the original threat-model concern explicitly.
    let parent_sid = unsafe { libc::getsid(0) };
    assert_ne!(
        sid, parent_sid,
        "worker session ({sid}) collided with parent session ({parent_sid}); \
         setsid() was probably not called"
    );
}

#[test]
fn net_is_unreachable_under_deny() {
    if skip_if_no_seatbelt() {
        return;
    }
    let probe = net_probe_binary();
    if !probe.exists() {
        eprintln!(
            "[SKIP] net_probe binary not built at {probe:?} — run `cargo build --workspace` first"
        );
        return;
    }
    // The probe binary needs to be readable inside the sandbox.
    let mut policy = strict_policy();
    policy.fs_read.push(probe.clone());

    let backend = MacosSeatbelt::new();
    let probe_str = probe.to_string_lossy().into_owned();
    let mut child = backend
        .spawn_under_policy(&policy, &probe_str, &[])
        .expect("sandbox-exec should spawn net_probe");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "net_probe should fail under Net::Deny (TCP connect blocked); stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
}
