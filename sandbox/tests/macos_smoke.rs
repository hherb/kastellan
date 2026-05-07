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
