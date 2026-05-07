//! End-to-end tests for the macOS Seatbelt backend. These actually invoke
//! `/usr/bin/sandbox-exec`, so they only run on macOS.

#![cfg(target_os = "macos")]

use std::io::Read;
#[allow(unused_imports)]
use std::path::PathBuf;

#[allow(unused_imports)]
use hhagent_sandbox::{macos_seatbelt::MacosSeatbelt, Net, Profile, SandboxBackend, SandboxPolicy};

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
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 64,
        profile: Profile::WorkerStrict,
        env: vec![],
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
    assert!(matches!(res, Err(hhagent_sandbox::SandboxError::Backend(_))));
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
