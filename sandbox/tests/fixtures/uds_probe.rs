//! Tiny Unix-domain-socket connectivity probe used by the Seatbelt
//! egress-proxy gating test. Built as a workspace bin; tests invoke
//! `target/debug/uds_probe` under a Seatbelt sandbox policy.
//!
//! Usage: `uds_probe <socket_path>`
//!
//! Exit codes:
//!   0 — `UnixStream::connect` succeeded (UDS accessible)
//!   1 — connect failed, or no argument supplied
//!
//! No I/O is performed after connecting — we only care whether the
//! AF_UNIX connect is allowed under the profile.

use std::os::unix::net::UnixStream;

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("uds_probe: usage: uds_probe <socket_path>");
            std::process::exit(1);
        }
    };
    let exit_code = match UnixStream::connect(&path) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("uds_probe: connect({path:?}) failed: {e}");
            1
        }
    };
    std::process::exit(exit_code);
}
