//! End-to-end tests for the Linux bwrap backend. These actually invoke `bwrap`,
//! so they only run on Linux and require `bwrap` on `$PATH`.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::PathBuf;

use hhagent_sandbox::{linux_bwrap::LinuxBwrap, Net, Profile, SandboxBackend, SandboxPolicy};

/// Skip the test if this host's kernel won't let us create an unprivileged
/// user namespace. Ubuntu 24.04+ requires an AppArmor profile for bwrap;
/// tests should report a clear hint rather than fail with an opaque error.
fn skip_if_no_userns() -> bool {
    match LinuxBwrap::probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
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
fn echo_runs_inside_sandbox() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/echo", &["hello-from-jail"])
        .expect("bwrap should spawn echo");
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
fn host_etc_passwd_is_invisible_when_not_in_policy() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    // /etc is not bound, so /etc/passwd should not exist inside the jail.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/cat", &["/etc/passwd"])
        .expect("bwrap should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "cat /etc/passwd should fail inside sandbox; stdout={} stderr={}",
        read_to_string(&mut child.stdout),
        read_to_string(&mut child.stderr)
    );
    let stderr = read_to_string(&mut child.stderr);
    assert!(
        stderr.to_lowercase().contains("no such file"),
        "expected 'No such file', got stderr: {stderr:?}"
    );
}

#[test]
fn host_home_is_invisible_when_not_in_policy() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    // The jail must not see the user's home dir under any circumstance unless
    // it was explicitly listed.
    let probe = "/home";
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/ls", &[probe])
        .expect("bwrap should spawn ls");
    let status = child.wait().expect("wait");
    let stdout = read_to_string(&mut child.stdout);
    let stderr = read_to_string(&mut child.stderr);
    // Either ls fails because /home doesn't exist, or it succeeds but lists
    // nothing. Both are acceptable; what's NOT acceptable is seeing real users.
    assert!(
        !stdout.contains("hherb"),
        "sandbox leaked the host's home directory! stdout={stdout:?}"
    );
    let _ = (status, stderr); // unused but kept for diagnostic context
}

#[test]
fn fs_read_path_is_visible_when_listed() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("/etc/hostname"));
    let mut child = backend
        .spawn_under_policy(&policy, "/usr/bin/cat", &["/etc/hostname"])
        .expect("bwrap should spawn cat");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "cat /etc/hostname should succeed when listed; stderr={}",
        read_to_string(&mut child.stderr)
    );
    let stdout = read_to_string(&mut child.stdout);
    assert!(!stdout.is_empty(), "expected non-empty hostname");
}

#[test]
fn net_is_unreachable_under_deny() {
    if skip_if_no_userns() {
        return;
    }
    let backend = LinuxBwrap::new();
    // `getent hosts ...` performs a DNS lookup, which requires the host
    // network namespace. With Net::Deny we unshare net, so this MUST fail.
    let mut child = backend
        .spawn_under_policy(&strict_policy(), "/usr/bin/getent", &["hosts", "example.com"])
        .expect("bwrap should spawn getent");
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "getent hosts should fail under Net::Deny — sandbox leaked the network namespace"
    );
}

#[test]
fn relative_policy_paths_are_rejected() {
    let backend = LinuxBwrap::new();
    let mut policy = strict_policy();
    policy.fs_read.push(PathBuf::from("relative/path"));
    let res = backend.spawn_under_policy(&policy, "/usr/bin/true", &[]);
    assert!(matches!(
        res,
        Err(hhagent_sandbox::SandboxError::Backend(_))
    ));
}
