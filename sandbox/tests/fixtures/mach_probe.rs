//! Tiny mach-lookup probe used by the macOS smoke test that verifies the
//! Seatbelt profile does NOT grant `mach-lookup` to workers (issue #1).
//! Built as a workspace bin; tests invoke `target/debug/mach_probe` under
//! a sandbox policy.
//!
//! Behaviour: attempts `bootstrap_look_up(bootstrap_port,
//! "com.apple.coreservices.appleevents", &port)`. Apple Events broker is a
//! deliberately benign-but-non-essential service to probe with — it's
//! present on every macOS host, but no shipping kastellan worker has any
//! legitimate reason to talk to it (it's the back-end for AppleScript /
//! distributed Apple-Event delivery, the canonical privilege-escalation
//! surface our threat model wants to keep workers away from).
//!
//! Exit codes:
//!   0 — lookup succeeded (sandbox allowed mach-lookup — BUG, must fail)
//!   1 — lookup denied / failed (correct under our deny-default mach policy)
//!
//! No std::env, no logging beyond stderr — the test reads only the exit
//! code. The mach symbols are part of libSystem and link with no extra flags.
//!
//! On non-macOS targets the fixture compiles to a stub that prints a
//! marker line and exits 1 — `cargo build --workspace` then succeeds
//! everywhere even though the fixture is only meaningful on macOS.
//! `[[bin]]` tables in `sandbox/Cargo.toml` do not support per-target
//! conditional inclusion, so source-level cfg is the canonical pattern.

#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::raw::c_char;
#[cfg(target_os = "macos")]
use std::process;

// Apple's `<servers/bootstrap.h>` types. `mach_port_t` is `u32` on every
// modern Darwin ABI (32-bit port name); `kern_return_t` is `i32`. We declare
// only what we need to avoid pulling in a heavy bindings crate just for the
// fixture.
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types)]
type mach_port_t = u32;
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types)]
type kern_return_t = i32;

#[cfg(target_os = "macos")]
extern "C" {
    /// Per-process bootstrap port handed to us by launchd at exec(2). This
    /// is a real symbol exported from libSystem.B.dylib — the linker resolves
    /// it without any extra `-l` flag.
    static bootstrap_port: mach_port_t;

    /// `bootstrap_look_up(bp, service_name, &mut sp) -> kern_return_t`
    /// — KERN_SUCCESS (0) on success, non-zero (typically
    /// `BOOTSTRAP_NOT_PRIVILEGED` or `BOOTSTRAP_UNKNOWN_SERVICE`) on failure.
    /// Under a Seatbelt profile that denies `mach-lookup`, the kernel
    /// short-circuits before the lookup ever reaches launchd, returning a
    /// failure code.
    fn bootstrap_look_up(
        bp: mach_port_t,
        service_name: *const c_char,
        sp: *mut mach_port_t,
    ) -> kern_return_t;
}

#[cfg(target_os = "macos")]
fn main() {
    let service = CString::new("com.apple.coreservices.appleevents")
        .expect("hardcoded ASCII name has no NUL");
    let mut port: mach_port_t = 0;
    // SAFETY: bootstrap_look_up is a C function with the standard mach
    // calling convention; we pass valid pointers to a CString and a stack
    // mach_port_t. bootstrap_port is read-only.
    let kr = unsafe { bootstrap_look_up(bootstrap_port, service.as_ptr(), &mut port) };
    if kr == 0 {
        println!("looked up: port={port}");
        process::exit(0);
    }
    eprintln!("bootstrap_look_up failed: kr={kr}");
    process::exit(1);
}

/// Non-macOS stub. Prints a marker line so anything that mistakenly
/// invokes the fixture on Linux gets a self-explanatory failure
/// instead of a confusing "the binary did the wrong thing" symptom.
/// Exit 1 mirrors the real fixture's "lookup denied" path.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mach_probe: stub on non-macOS target — fixture is meaningful only on macOS");
    std::process::exit(1);
}
