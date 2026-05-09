//! Tiny session-id probe used by smoke tests to verify that the macOS
//! Seatbelt backend places its worker in a *new session* (not just a new
//! process group). Built as a workspace bin; tests invoke
//! `target/debug/sid_probe` under a sandbox policy.
//!
//! Output (single line on stdout): `<pid> <sid>` where both are decimal,
//! whitespace-separated. Exit code 0 on success, 1 on syscall failure.
//!
//! No std::env, no logging — the test parses two integers and compares.

use std::process;

fn main() {
    // SAFETY: getpid/getsid take no arguments and only read kernel state.
    // getsid(0) returns the calling process's session ID.
    let pid = unsafe { libc::getpid() };
    let sid = unsafe { libc::getsid(0) };
    if sid < 0 {
        eprintln!("getsid(0) failed");
        process::exit(1);
    }
    println!("{pid} {sid}");
}
