//! Tiny network-reachability probe used by smoke tests on platforms that
//! don't have `getent` (i.e. macOS). Built as a workspace bin; tests
//! invoke `target/debug/net_probe` under a sandbox policy.
//!
//! Exit codes:
//!   0 — TCP connect succeeded (network reachable)
//!   1 — connect failed or timed out (network blocked / unreachable)
//!
//! No std::env, no logging, no DNS — connects to a literal IP so the test
//! is deterministic on offline machines and tells us about the *network*
//! layer, not the resolver.

use std::net::TcpStream;
use std::time::Duration;

fn main() {
    let addr = "1.1.1.1:443"
        .parse()
        .expect("hardcoded socket address parses");
    let exit_code = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => 0,
        Err(_) => 1,
    };
    std::process::exit(exit_code);
}
