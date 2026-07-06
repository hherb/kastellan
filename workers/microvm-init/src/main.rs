//! PID1 inside the Firecracker guest. Mounts the minimal pseudo-filesystems,
//! accepts the host's JSON-RPC bridge over AF_VSOCK, wires it onto the worker's
//! fd 0/1, and execs the worker. The worker (`serve_stdio`) is UNCHANGED — this
//! init performs the vsock<->stdio adaptation so the worker still "speaks stdio".
//!
//! The worker binary path + env arrive via the kernel cmdline / a baked config
//! (see WORKER_CMD). Slice 1 bakes the python-exec worker invocation.
//!
//! This crate is guest-only (Linux). On macOS the binary stubs out with an
//! error message so `cargo build --workspace` stays green on the dev box.
//!
//! # Layout (Item 9b prod-split, 2026-07-06)
//!
//! The former single 1052-LOC `main.rs` was split by concern; the bodies moved
//! verbatim (privates widened to `pub(crate)` for cross-module use):
//! - [`cmdline`] — the pure, cross-platform kernel-cmdline "wire contract":
//!   the token consts (kept in sync with `kastellan-sandbox`), the fail-safe
//!   parsers, the value types, and their unit tests (Mac-runnable).
//! - [`guest`] (Linux-only) — the real PID1 syscall mechanism: pseudo-fs +
//!   host-share mounts, loopback bring-up, the vsock bridge accept, and the
//!   worker `exec`, plus the slice-4a egress relay (`guest::egress`).
//!
//! This file keeps only the module wiring and the two `main` entry points.

mod cmdline;

#[cfg(target_os = "linux")]
mod guest;

#[cfg(target_os = "linux")]
use cmdline::{parse_egress_config, parse_mount_manifest};
#[cfg(target_os = "linux")]
use guest::{
    accept_host_bridge, apply_host_mounts, bring_loopback_up, egress_selftest, exec_worker,
    mount_pseudo_fs, setup_egress_relay,
};

// ── Entry points ──────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn main() {
    mount_pseudo_fs();
    // Guest `lo` boots DOWN; the matrix worker's ProxyBridge binds 127.0.0.1.
    // Unconditional + harmless for loopback-free workers (slice 5b-4b).
    bring_loopback_up();
    let cmdline_for_mounts = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    apply_host_mounts(&parse_mount_manifest(&cmdline_for_mounts));
    let egress = parse_egress_config(&cmdline_for_mounts);
    if egress.enabled {
        setup_egress_relay();
        if egress.selftest {
            egress_selftest();
        }
    }
    let conn_fd = accept_host_bridge();
    // Redirect the worker's stdio onto the vsock connection. A silent dup2
    // failure here would boot the guest with a dead JSON-RPC bridge and no
    // diagnostic, so the returns are checked (#361). dup2 returns the new fd
    // number on success.
    unsafe {
        assert_eq!(libc::dup2(conn_fd, 0), 0, "dup2 vsock -> stdin");
        assert_eq!(libc::dup2(conn_fd, 1), 1, "dup2 vsock -> stdout");
        // conn_fd is now duplicated onto fd 0 and 1; close the original so the
        // worker does not inherit a third copy (#361).
        if conn_fd > 1 {
            libc::close(conn_fd);
        }
    }
    // exec the worker (baked path + args); env from the baked config.
    exec_worker();
}

/// Stub for non-Linux platforms so `cargo build --workspace` on macOS stays green.
/// This binary is guest-only and will never run on macOS.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("kastellan-microvm-init runs only inside a Linux guest");
    std::process::exit(1);
}
