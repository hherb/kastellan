# Egress Proxy — Slice #1 (Boundary Enforcement + SSRF) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a sandboxed per-worker egress proxy that enforces a host allowlist at the boundary and closes the SSRF/DNS-rebinding gap (resolve-then-pin, allowlist-aware private-IP deny), proven end-to-end by a test CONNECT client — without routing real workers through it yet (that lands in slice #2).

**Architecture:** A new `workers/egress-proxy` binary listens on a per-worker UDS, parses `CONNECT host:port`, checks `web-common::HostAllowlist`, resolves DNS itself, rejects denied IP ranges (with a literal-IP carve-out), dials the pinned IP, and tunnels. It runs under a new `Net::ProxyEgress` sandbox policy. A reusable `core/src/egress` module spawns the sandboxed sidecar and maps its stdout decision stream to audit rows. `tool_host` is **not** modified this slice.

**Tech Stack:** Rust, `std::net` + `std::os::unix::net` (blocking, threads), `web-common::HostAllowlist`, `worker-prelude::lock_down`, the `SandboxBackend` trait, `serde_json`.

**Spec:** [`docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md`](../specs/2026-06-10-egress-proxy-boundary-enforcement-design.md)

**Branch:** `feat/egress-proxy-boundary` (already created; spec is committed there).

**Standing rules:** `source "$HOME/.cargo/env"` before any cargo command. Keep files < 500 LOC. All tests pass before each commit. Commit specific files (never `git add -A` — keep `docs/essay-medium-draft.md` and `.claude/*.lock` out). End commit messages with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer.

---

## File Structure

**New crate `workers/egress-proxy`:**
- `Cargo.toml` — bin crate; deps `kastellan-worker-web-common`, `kastellan-worker-prelude`, `serde`, `serde_json`, `anyhow`.
- `src/ssrf.rs` — pure `is_denied_range(IpAddr) -> bool` + tests.
- `src/request_line.rs` — pure CONNECT-line parse → `(host, port)` + tests.
- `src/report.rs` — `Decision` + `Verdict` enums, JSON-line serialization, `Reporter` seam + tests.
- `src/proxy.rs` — `Resolve` seam (`StdResolve`) + `handle_conn` drive function + hermetic tests.
- `src/main.rs` — env parse, UDS bind, `lock_down`, accept loop.

**Sandbox:**
- `sandbox/src/lib.rs` — add `Net::ProxyEgress` variant.
- `sandbox/src/linux_bwrap.rs` — extend the `--share-net` match.
- `sandbox/src/macos_seatbelt.rs` — extend the `(allow network*)` match.
- `sandbox/src/macos_container.rs` — add the exhaustive-match arm.

**Core host side:**
- `core/src/egress/mod.rs` — module facade.
- `core/src/egress/audit.rs` — pure `decision_to_audit`.
- `core/src/egress/spawn.rs` — `spawn_sidecar` + `SidecarHandle`.
- `core/src/lib.rs` — `pub mod egress;`.
- `core/tests/egress_proxy_e2e.rs` — real sandbox + test CONNECT client + PG-gated audit insert.

**Docs:** `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`.

---

## Task 1: Scaffold the `egress-proxy` crate

**Files:**
- Create: `workers/egress-proxy/Cargo.toml`
- Create: `workers/egress-proxy/src/main.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Add the crate to the workspace members**

In root `Cargo.toml`, the `members` array currently ends with `"workers/web-search",`. Add a line after it:

```toml
    "workers/web-search",
    "workers/egress-proxy",
```

- [ ] **Step 2: Write `workers/egress-proxy/Cargo.toml`**

```toml
[package]
name        = "kastellan-worker-egress-proxy"
description = "Per-worker egress proxy: host-allowlist + SSRF/IP-pinning boundary enforcement over a UDS. Slice #1 (no TLS interception)."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme.workspace       = true

[[bin]]
name = "kastellan-worker-egress-proxy"
path = "src/main.rs"

[dependencies]
kastellan-worker-prelude    = { path = "../prelude" }
kastellan-worker-web-common = { path = "../web-common" }
serde      = { workspace = true }
serde_json = { workspace = true }
anyhow     = { workspace = true }
```

- [ ] **Step 3: Write a placeholder `workers/egress-proxy/src/main.rs`**

```rust
//! egress-proxy: a per-worker egress boundary. Listens on a UDS, enforces the
//! worker's host allowlist + SSRF/IP defense per CONNECT, tunnels to the pinned
//! IP. Slice #1: no TLS interception, no live worker routing.
//! Design: docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md

fn main() -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 4: Build the workspace**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-egress-proxy`
Expected: compiles clean (one `unused` warning is fine for the placeholder).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml workers/egress-proxy/Cargo.toml workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): scaffold crate (ROADMAP:141, slice #1)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: SSRF range classifier (`ssrf.rs`)

The security core. `is_denied_range(ip)` returns `true` for any address a hostname must **not** be allowed to resolve to. The literal-IP carve-out is **not** here — it lives in `proxy.rs` (a literal-IP CONNECT target that the allowlist accepts skips this check entirely).

**Files:**
- Create: `workers/egress-proxy/src/ssrf.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod ssrf;`)

- [ ] **Step 1: Write the failing tests**

Create `workers/egress-proxy/src/ssrf.rs`:

```rust
//! SSRF range classifier. `is_denied_range` is the single security-critical
//! predicate: it returns true for every address class a *hostname* must not be
//! permitted to resolve to (the DNS-rebinding defense). Literal-IP CONNECT
//! targets are handled by the carve-out in `proxy.rs`, not here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True iff `ip` is in a range we refuse to connect a *resolved hostname* to.
/// Covers loopback, RFC1918 private, link-local, unique-local, CGNAT,
/// multicast, unspecified, and IPv4-mapped-IPv6 (unwrapped + re-checked).
pub fn is_denied_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_denied_v4(v4),
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) must be unwrapped so a mapped
            // private address can't slip past as "just an IPv6 global".
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_denied_v4(v4);
            }
            is_denied_v6(v6)
        }
    }
}

fn is_denied_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()            // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254.0.0/16
        || ip.is_multicast()    // 224.0.0.0/4
        || ip.is_unspecified()  // 0.0.0.0
        || ip.is_broadcast()    // 255.255.255.255
        || is_cgnat_v4(ip)      // 100.64.0.0/10
}

/// RFC6598 carrier-grade NAT space (`is_shared` is unstable in std, so inline).
fn is_cgnat_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

fn is_denied_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()            // ::1
        || ip.is_unspecified()  // ::
        || ip.is_multicast()    // ff00::/8
        || is_unique_local_v6(ip) // fc00::/7
        || is_link_local_v6(ip)   // fe80::/10
}

/// fc00::/7 (unique-local). `Ipv6Addr::is_unique_local` is unstable; inline.
fn is_unique_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// fe80::/10 (link-local). `Ipv6Addr::is_unicast_link_local` is unstable; inline.
fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    #[test]
    fn public_v4_is_allowed() {
        assert!(!is_denied_range(v4("203.0.113.5")));
        assert!(!is_denied_range(v4("8.8.8.8")));
    }

    #[test]
    fn private_and_loopback_v4_are_denied() {
        for s in ["127.0.0.1", "10.0.0.1", "172.16.5.5", "192.168.1.1",
                  "169.254.1.1", "100.64.0.1", "224.0.0.1", "0.0.0.0",
                  "255.255.255.255"] {
            assert!(is_denied_range(v4(s)), "{s} should be denied");
        }
    }

    #[test]
    fn cgnat_boundaries() {
        assert!(is_denied_range(v4("100.64.0.0")));
        assert!(is_denied_range(v4("100.127.255.255")));
        assert!(!is_denied_range(v4("100.63.255.255")));
        assert!(!is_denied_range(v4("100.128.0.0")));
    }

    #[test]
    fn public_v6_is_allowed() {
        assert!(!is_denied_range(v6("2606:4700:4700::1111")));
    }

    #[test]
    fn private_and_loopback_v6_are_denied() {
        for s in ["::1", "::", "ff02::1", "fc00::1", "fd12:3456::1", "fe80::1"] {
            assert!(is_denied_range(v6(s)), "{s} should be denied");
        }
    }

    #[test]
    fn ipv4_mapped_private_is_denied() {
        // ::ffff:10.0.0.1 must be unwrapped and denied.
        assert!(is_denied_range(v6("::ffff:10.0.0.1")));
        // ::ffff:8.8.8.8 unwraps to a public v4 → allowed.
        assert!(!is_denied_range(v6("::ffff:8.8.8.8")));
    }
}
```

In `workers/egress-proxy/src/main.rs`, add at the top (after the doc comment):

```rust
mod ssrf;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy ssrf`
Expected: FAIL — actually, since the implementation is written alongside the tests in this file, this task is a special case. Run the build first: if `is_denied_range` had a typo it would fail to compile. (For strict TDD, you may comment out the function bodies to `todo!()` first, watch them panic, then restore.)

- [ ] **Step 3: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy ssrf`
Expected: PASS — 6 tests.

- [ ] **Step 4: Commit**

```bash
git add workers/egress-proxy/src/ssrf.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): SSRF range classifier with exhaustive range tests (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: CONNECT request-line parser (`request_line.rs`)

**Files:**
- Create: `workers/egress-proxy/src/request_line.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod request_line;`)

- [ ] **Step 1: Write the parser + tests**

Create `workers/egress-proxy/src/request_line.rs`:

```rust
//! Pure parse of an HTTP `CONNECT` request line into `(host, port)`.
//! The slice-#2 web-common connector issues `CONNECT` for both http and https,
//! so the proxy only ever parses CONNECT. Handles bracketed IPv6 authorities.

/// Parse `CONNECT host:port HTTP/1.1` (the leading line of a CONNECT tunnel
/// request). Returns the host (brackets stripped for IPv6) and port.
/// Returns `Err` for anything that isn't a well-formed CONNECT line.
pub fn parse_connect(line: &str) -> Result<(String, u16), String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    if method != "CONNECT" {
        return Err(format!("not a CONNECT request: {method:?}"));
    }
    let authority = parts.next().ok_or_else(|| "missing authority".to_string())?;
    // Reject a missing HTTP version (too-short line).
    if parts.next().is_none() {
        return Err("missing HTTP version".to_string());
    }
    split_authority(authority)
}

/// Split `host:port` — handling `[::1]:443` bracketed IPv6 — into parts.
fn split_authority(authority: &str) -> Result<(String, u16), String> {
    let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: [host]:port
        let close = rest.find(']').ok_or_else(|| "unterminated [".to_string())?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| "missing :port after ]".to_string())?;
        (host.to_string(), port)
    } else {
        let (h, p) = authority
            .rsplit_once(':')
            .ok_or_else(|| "missing :port".to_string())?;
        (h.to_string(), p)
    };
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    let port: u16 = port_str.parse().map_err(|_| format!("bad port {port_str:?}"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hostname() {
        assert_eq!(
            parse_connect("CONNECT api.example.com:443 HTTP/1.1").unwrap(),
            ("api.example.com".to_string(), 443)
        );
    }

    #[test]
    fn parses_literal_v4() {
        assert_eq!(
            parse_connect("CONNECT 127.0.0.1:8888 HTTP/1.1\r\n").unwrap(),
            ("127.0.0.1".to_string(), 8888)
        );
    }

    #[test]
    fn parses_bracketed_v6() {
        assert_eq!(
            parse_connect("CONNECT [::1]:443 HTTP/1.1").unwrap(),
            ("::1".to_string(), 443)
        );
    }

    #[test]
    fn rejects_non_connect() {
        assert!(parse_connect("GET / HTTP/1.1").is_err());
    }

    #[test]
    fn rejects_missing_port() {
        assert!(parse_connect("CONNECT example.com HTTP/1.1").is_err());
    }

    #[test]
    fn rejects_missing_version() {
        assert!(parse_connect("CONNECT example.com:443").is_err());
    }

    #[test]
    fn rejects_bad_port() {
        assert!(parse_connect("CONNECT example.com:notaport HTTP/1.1").is_err());
    }
}
```

Add `mod request_line;` to `main.rs`.

- [ ] **Step 2: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy request_line`
Expected: PASS — 7 tests.

- [ ] **Step 3: Commit**

```bash
git add workers/egress-proxy/src/request_line.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): CONNECT request-line parser (incl. bracketed IPv6) (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Decision record + reporter seam (`report.rs`)

**Files:**
- Create: `workers/egress-proxy/src/report.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod report;`)

- [ ] **Step 1: Write the record + tests**

Create `workers/egress-proxy/src/report.rs`:

```rust
//! Per-request decision record and the `Reporter` seam. In production the
//! reporter writes one JSON line per decision to stdout; tests collect records
//! in a `Vec` to assert on. Core ingests the JSON stream and persists audit
//! rows (the proxy never touches Postgres — core-only-DB invariant).

use serde::Serialize;

/// The policy outcome for one CONNECT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allowed,
    BlockedAllowlist,
    BlockedSsrf,
}

/// One decision, serialized as a single JSON line on stdout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub worker: String,
    pub host: String,
    pub port: u16,
    /// The pinned resolved IP, when one was chosen (`None` for blocks/no target).
    pub resolved_ip: Option<String>,
    pub verdict: Verdict,
    pub reason: String,
}

impl Decision {
    /// Serialize to a single line (no embedded newlines — `serde_json::to_string`
    /// never emits them for these scalar fields).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("Decision serialization never fails")
    }
}

/// Sink for decisions. Production = stdout writer; tests = a Vec collector.
pub trait Reporter {
    fn report(&mut self, decision: Decision);
}

/// Writes one JSON line per decision to the provided writer (stdout in prod).
pub struct LineReporter<W: std::io::Write> {
    pub out: W,
}

impl<W: std::io::Write> Reporter for LineReporter<W> {
    fn report(&mut self, decision: Decision) {
        // Best-effort: a broken stdout pipe must not crash the proxy mid-tunnel.
        let _ = writeln!(self.out, "{}", decision.to_line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_line_shape() {
        let d = Decision {
            worker: "web-fetch".into(),
            host: "api.example.com".into(),
            port: 443,
            resolved_ip: Some("203.0.113.5".into()),
            verdict: Verdict::Allowed,
            reason: "ok".into(),
        };
        let v: serde_json::Value = serde_json::from_str(&d.to_line()).unwrap();
        assert_eq!(v["verdict"], "allowed");
        assert_eq!(v["host"], "api.example.com");
        assert_eq!(v["resolved_ip"], "203.0.113.5");
    }

    #[test]
    fn blocked_verdicts_serialize_snake_case() {
        let mk = |verdict| Decision {
            worker: "web-fetch".into(),
            host: "h".into(),
            port: 443,
            resolved_ip: None,
            verdict,
            reason: "r".into(),
        };
        assert!(mk(Verdict::BlockedAllowlist).to_line().contains("\"blocked_allowlist\""));
        assert!(mk(Verdict::BlockedSsrf).to_line().contains("\"blocked_ssrf\""));
    }

    #[test]
    fn vec_reporter_collects() {
        let mut buf = Vec::new();
        {
            let mut r = LineReporter { out: &mut buf };
            r.report(Decision {
                worker: "w".into(), host: "h".into(), port: 1,
                resolved_ip: None, verdict: Verdict::Allowed, reason: "ok".into(),
            });
        }
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.lines().count(), 1);
    }
}
```

Add `mod report;` to `main.rs`.

- [ ] **Step 2: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy report`
Expected: PASS — 3 tests.

- [ ] **Step 3: Commit**

```bash
git add workers/egress-proxy/src/report.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): decision record + JSON-line reporter seam (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: The drive loop (`proxy.rs`)

The heart: parse CONNECT → allowlist → resolve (via an injectable `Resolve` seam) → SSRF filter with the literal-IP carve-out → dial pinned IP → tunnel. The seam lets the SSRF block path be tested hermetically (stub a public-looking name to a private IP).

**Files:**
- Create: `workers/egress-proxy/src/proxy.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod proxy;`)

- [ ] **Step 1: Write the drive function**

Create `workers/egress-proxy/src/proxy.rs`:

```rust
//! The per-connection drive loop: parse CONNECT, enforce allowlist + SSRF,
//! pin the resolved IP, and tunnel. Pure decision logic is separated from I/O
//! so the policy paths are unit-testable; `handle_conn` does the byte-shuffling.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;

use kastellan_worker_web_common::allowlist::HostAllowlist;

use crate::report::{Decision, Reporter, Verdict};
use crate::request_line::parse_connect;
use crate::ssrf::is_denied_range;

/// DNS seam: real impl resolves via getaddrinfo; tests stub it.
pub trait Resolve: Send + Sync {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
}

/// Production resolver — `std::net` getaddrinfo.
pub struct StdResolve;

impl Resolve for StdResolve {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        Ok((host, port).to_socket_addrs()?.map(|s| s.ip()).collect())
    }
}

/// The policy decision for a parsed CONNECT target, before any dialing.
/// Separated from I/O so it is exhaustively unit-testable.
pub enum Target {
    /// Connect to this exact pinned address.
    Dial(IpAddr),
    /// Refuse; carry the verdict + reason for the decision record.
    Block(Verdict, String),
}

/// Decide what to do with `host:port`, given the allowlist and a resolver.
/// - Literal-IP target: allowed iff the allowlist accepts the literal string;
///   the SSRF range check is **skipped** (operator allowlisted that exact addr).
/// - Hostname target: allowed iff the allowlist accepts the name; then every
///   resolved IP is range-checked and the first non-denied one is pinned.
pub fn decide(host: &str, port: u16, allow: &HostAllowlist, resolver: &dyn Resolve) -> Target {
    if !allow.is_allowed(host) {
        return Target::Block(Verdict::BlockedAllowlist, format!("{host} not on allowlist"));
    }
    if let Ok(literal) = host.parse::<IpAddr>() {
        // Operator-allowlisted literal address — intent is explicit; no DNS,
        // no range deny (this is the local-SearxNG 127.0.0.1 carve-out).
        return Target::Dial(literal);
    }
    let ips = match resolver.resolve(host, port) {
        Ok(ips) if !ips.is_empty() => ips,
        Ok(_) => return Target::Block(Verdict::BlockedSsrf, format!("{host} resolved to nothing")),
        Err(e) => return Target::Block(Verdict::BlockedSsrf, format!("resolve failed: {e}")),
    };
    match ips.into_iter().find(|ip| !is_denied_range(*ip)) {
        Some(ip) => Target::Dial(ip),
        None => Target::Block(Verdict::BlockedSsrf, format!("{host} resolves only to denied ranges")),
    }
}

/// Handle one accepted UDS connection end-to-end. Reads the CONNECT line,
/// decides, and on `Dial` pins the IP, replies 200, and bidi-copies until EOF.
/// Always emits exactly one decision to `reporter`.
pub fn handle_conn(
    mut client: UnixStream,
    worker: &str,
    allow: &HostAllowlist,
    resolver: &dyn Resolve,
    reporter: &mut dyn Reporter,
) {
    let line = match read_request_line(&mut client) {
        Ok(l) => l,
        Err(_) => {
            reporter.report(blocked(worker, "", 0, Verdict::BlockedAllowlist, "unreadable request"));
            return;
        }
    };
    let (host, port) = match parse_connect(&line) {
        Ok(hp) => hp,
        Err(e) => {
            reporter.report(blocked(worker, "", 0, Verdict::BlockedAllowlist, &format!("parse: {e}")));
            let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            return;
        }
    };

    match decide(&host, port, allow, resolver) {
        Target::Block(verdict, reason) => {
            reporter.report(blocked(worker, &host, port, verdict, &reason));
            let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n");
        }
        Target::Dial(ip) => {
            let upstream = match TcpStream::connect((ip, port)) {
                Ok(s) => s,
                Err(e) => {
                    // Transport failure, not a policy verdict: decision was allowed.
                    reporter.report(Decision {
                        worker: worker.into(), host: host.clone(), port,
                        resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                        reason: format!("connect_failed: {e}"),
                    });
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                    return;
                }
            };
            reporter.report(Decision {
                worker: worker.into(), host: host.clone(), port,
                resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed, reason: "ok".into(),
            });
            if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").is_ok() {
                tunnel(client, upstream);
            }
        }
    }
}

fn blocked(worker: &str, host: &str, port: u16, verdict: Verdict, reason: &str) -> Decision {
    Decision {
        worker: worker.into(), host: host.into(), port,
        resolved_ip: None, verdict, reason: reason.into(),
    }
}

/// Read just the CONNECT request line (up to the first CRLF), then drain the
/// remaining header block up to the blank line so the tunnel starts clean.
fn read_request_line(client: &mut UnixStream) -> std::io::Result<String> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    // Drain the rest of the header block (CONNECT requests may carry headers).
    let mut header = String::new();
    loop {
        header.clear();
        let n = reader.read_line(&mut header)?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }
    Ok(line)
}

/// Bidirectional copy between the client UDS and the upstream TCP stream until
/// either side closes. One thread per direction.
fn tunnel(client: UnixStream, upstream: TcpStream) {
    let (c_read, c_write) = (client.try_clone(), client);
    let (u_read, u_write) = (upstream.try_clone(), upstream);
    let (Ok(mut cr), Ok(mut ur)) = (c_read, u_read) else { return };
    let mut cw = c_write;
    let mut uw = u_write;
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut cr, &mut uw);
        let _ = uw.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut ur, &mut cw);
    let _ = cw.shutdown(std::net::Shutdown::Both);
    let _ = up.join();
}

#[cfg(test)]
mod tests;
```

- [ ] **Step 2: Write the hermetic tests in a sibling `proxy/tests.rs`**

Create `workers/egress-proxy/src/proxy/tests.rs`:

```rust
//! Hermetic drive-loop tests. `decide` is tested directly (pure); `handle_conn`
//! is driven over a real UDS with a test CONNECT client against a localhost
//! origin, with a stubbed resolver for the SSRF path.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener};
use std::os::unix::net::{UnixListener, UnixStream};

use kastellan_worker_web_common::allowlist::HostAllowlist;

use super::*;
use crate::report::{Decision, Reporter, Verdict};

fn al(entries: &[&str]) -> HostAllowlist {
    HostAllowlist::from_env_json(&serde_json::to_string(entries).unwrap()).unwrap()
}

struct StubResolve(Vec<IpAddr>);
impl Resolve for StubResolve {
    fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct VecReporter(Vec<Decision>);
impl Reporter for VecReporter {
    fn report(&mut self, d: Decision) { self.0.push(d); }
}

#[test]
fn decide_blocks_off_allowlist() {
    let r = StubResolve(vec!["203.0.113.5".parse().unwrap()]);
    match decide("evil.test", 443, &al(&["good.test"]), &r) {
        Target::Block(Verdict::BlockedAllowlist, _) => {}
        _ => panic!("expected allowlist block"),
    }
}

#[test]
fn decide_blocks_rebinding_to_private() {
    // public-looking name on the allowlist resolving to a private IP → SSRF block.
    let r = StubResolve(vec!["10.0.0.1".parse().unwrap()]);
    match decide("blocked.test", 443, &al(&["blocked.test"]), &r) {
        Target::Block(Verdict::BlockedSsrf, _) => {}
        _ => panic!("expected SSRF block"),
    }
}

#[test]
fn decide_allows_literal_loopback_when_allowlisted() {
    // The local-SearxNG carve-out: literal 127.0.0.1 explicitly allowlisted.
    let r = StubResolve(vec![]); // resolver must NOT be consulted for a literal.
    match decide("127.0.0.1", 8888, &al(&["127.0.0.1"]), &r) {
        Target::Dial(ip) => assert_eq!(ip, "127.0.0.1".parse::<IpAddr>().unwrap()),
        _ => panic!("expected dial to literal loopback"),
    }
}

#[test]
fn decide_pins_first_public_ip() {
    let r = StubResolve(vec!["10.0.0.1".parse().unwrap(), "203.0.113.9".parse().unwrap()]);
    match decide("ok.test", 443, &al(&["ok.test"]), &r) {
        Target::Dial(ip) => assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap()),
        _ => panic!("expected dial to first non-denied IP"),
    }
}

#[test]
fn handle_conn_tunnels_allowed_literal_origin() {
    // Origin: a localhost TCP server that echoes a fixed response after reading.
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_thread = std::thread::spawn(move || {
        let (mut s, _) = origin.accept().unwrap();
        let mut buf = [0u8; 16];
        let _ = s.read(&mut buf);
        s.write_all(b"HELLO").unwrap();
    });

    // Proxy: bind a UDS, accept one connection, handle it.
    let dir = std::env::temp_dir().join(format!("egress-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("egress.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let allow = al(&["127.0.0.1"]);
    let proxy_thread = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reporter = VecReporter::default();
        handle_conn(conn, "web-fetch", &allow, &StdResolve, &mut reporter);
        reporter.0
    });

    // Client: CONNECT to the literal-allowlisted origin, then read the echo.
    let mut client = UnixStream::connect(&sock).unwrap();
    write!(client, "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").unwrap();
    let mut head = [0u8; 39]; // "HTTP/1.1 200 Connection Established\r\n\r\n"
    client.read_exact(&mut head).unwrap();
    assert!(std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"));
    client.write_all(b"ping").unwrap();
    let mut echo = [0u8; 5];
    client.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"HELLO");

    origin_thread.join().unwrap();
    let decisions = proxy_thread.join().unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].verdict, Verdict::Allowed);
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn handle_conn_reports_block_for_off_allowlist() {
    let dir = std::env::temp_dir().join(format!("egress-test-block-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("egress.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let allow = al(&["good.test"]);
    let proxy_thread = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reporter = VecReporter::default();
        handle_conn(conn, "web-fetch", &allow, &StdResolve, &mut reporter);
        reporter.0
    });

    let mut client = UnixStream::connect(&sock).unwrap();
    write!(client, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    let _ = client.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp:?}");

    let decisions = proxy_thread.join().unwrap();
    assert_eq!(decisions[0].verdict, Verdict::BlockedAllowlist);
    let _ = std::fs::remove_file(&sock);
}
```

Add `mod proxy;` to `main.rs`.

- [ ] **Step 3: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy proxy`
Expected: PASS — 6 tests (4 `decide` + 2 `handle_conn`).

- [ ] **Step 4: Confirm `proxy.rs` is under the 500-LOC cap**

Run: `wc -l workers/egress-proxy/src/proxy.rs`
Expected: well under 500 (tests live in the sibling `proxy/tests.rs`).

- [ ] **Step 5: Commit**

```bash
git add workers/egress-proxy/src/proxy.rs workers/egress-proxy/src/proxy/tests.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): CONNECT drive loop with allowlist + SSRF pinning + literal-IP carve-out (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Wire `main.rs` (env parse, UDS bind, lockdown, accept loop)

**Files:**
- Modify: `workers/egress-proxy/src/main.rs`

- [ ] **Step 1: Replace `main.rs` body**

```rust
//! egress-proxy: a per-worker egress boundary. Listens on a UDS, enforces the
//! worker's host allowlist + SSRF/IP defense per CONNECT, tunnels to the pinned
//! IP. Slice #1: no TLS interception, no live worker routing.
//! Design: docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md
//!
//! Env contract (set by the host-side `core::egress::spawn_sidecar`):
//!   KASTELLAN_EGRESS_PROXY_UDS       — absolute path of the UDS to bind.
//!   KASTELLAN_EGRESS_PROXY_ALLOWLIST — JSON array of allowed host strings.
//!   KASTELLAN_EGRESS_PROXY_WORKER    — the calling worker's name (for audit).

mod proxy;
mod report;
mod request_line;
mod ssrf;

use std::os::unix::net::UnixListener;

use kastellan_worker_web_common::allowlist::HostAllowlist;

use proxy::{handle_conn, StdResolve};
use report::LineReporter;

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_UDS unset"))?;
    let allow_json = std::env::var("KASTELLAN_EGRESS_PROXY_ALLOWLIST")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_ALLOWLIST unset"))?;
    let worker = std::env::var("KASTELLAN_EGRESS_PROXY_WORKER").unwrap_or_else(|_| "unknown".into());
    let allow = HostAllowlist::from_env_json(&allow_json)?;

    // Bind the UDS *before* lock-down (Landlock will forbid fs mutation after).
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS,
    // where the parent Seatbelt profile contains us). Outbound socket(2) +
    // AF_UNIX accept must remain permitted — see the net_client profile.
    // NOTE (Linux verification, run on the DGX): confirm the seccomp profile
    // permits AF_UNIX bind/listen/accept *and* AF_INET connect for a process
    // that both serves and dials; widen `seccomp_lock` if `accept` is refused.
    let _report = kastellan_worker_prelude::lock_down()?;

    let resolver = StdResolve;
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let allow = &allow;
        let worker = worker.clone();
        // One thread per connection; the proxy is SingleUse + short-lived.
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut reporter = LineReporter { out: std::io::stdout().lock() };
                handle_conn(conn, &worker, allow, &resolver, &mut reporter);
            });
        });
    }
    Ok(())
}
```

> **Implementation note:** `thread::scope` per-iteration keeps `&allow` borrows sound without `Arc`. If profiling later shows the scope-join serializes connections undesirably, switch to `Arc<HostAllowlist>` + detached threads — but for a `SingleUse` net worker the simplest correct form is fine. Do not add that complexity now (YAGNI).

- [ ] **Step 2: Build + run the full crate test suite**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-egress-proxy && cargo test -p kastellan-worker-egress-proxy`
Expected: builds clean; all unit tests pass.

- [ ] **Step 3: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-egress-proxy --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): main — env parse, UDS bind, lockdown, accept loop (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Add `Net::ProxyEgress` across the three sandbox backends

**Files:**
- Modify: `sandbox/src/lib.rs` (enum + a default-match test)
- Modify: `sandbox/src/linux_bwrap.rs` (`--share-net` match + test)
- Modify: `sandbox/src/macos_seatbelt.rs` (`(allow network*)` match + test)
- Modify: `sandbox/src/macos_container.rs` (exhaustive-match arm + test)

- [ ] **Step 1: Add the variant in `sandbox/src/lib.rs`**

In the `pub enum Net` block, after the `Allowlist(Vec<String>)` variant, add:

```rust
    /// The egress proxy itself: real outbound + DNS, self-enforcing. Maps to
    /// the same "share the host network namespace" behaviour as `Allowlist`
    /// *today*, but names the proxy-vs-worker distinction explicitly. Slice #2
    /// diverges them: `Allowlist` workers get a private netns whose only route
    /// out is the proxy UDS, while `ProxyEgress` keeps the real netns.
    ProxyEgress,
```

- [ ] **Step 2: Extend the bwrap match in `sandbox/src/linux_bwrap.rs`**

Change (around line 158):

```rust
    if matches!(policy.net, Net::Allowlist(_)) {
```

to:

```rust
    if matches!(policy.net, Net::Allowlist(_) | Net::ProxyEgress) {
```

- [ ] **Step 3: Extend the Seatbelt match in `sandbox/src/macos_seatbelt.rs`**

Change (around line 321):

```rust
    if matches!(policy.net, crate::Net::Allowlist(_)) {
```

to:

```rust
    if matches!(policy.net, crate::Net::Allowlist(_) | crate::Net::ProxyEgress) {
```

- [ ] **Step 4: Add the exhaustive-match arm in `sandbox/src/macos_container.rs`**

In the `match &policy.net { … }` block (around line 182), add a `ProxyEgress` arm alongside `Allowlist`:

```rust
        Net::Allowlist(_) | Net::ProxyEgress => {
            // The allowlist itself is enforced by the egress proxy worker, not
            // by `container` — same split as bwrap's `--share-net`. ProxyEgress
            // is the proxy's own policy (real netns); Allowlist is a worker's.
            argv.push("--network".into());
            argv.push("default".into());
        }
```

(Replace the existing `Net::Allowlist(_) => { … }` arm with this combined arm.)

- [ ] **Step 5: Add builder-shape tests**

In `sandbox/src/linux_bwrap.rs` tests module, add (mirroring the existing `Net::Allowlist` → `--share-net` test around line 240):

```rust
    #[test]
    fn proxy_egress_shares_net_like_allowlist() {
        let mut p = SandboxPolicy::default();
        p.net = Net::ProxyEgress;
        let argv = build_argv(&p, "/bin/true", &[]);
        assert!(argv.contains(&"--share-net".into()));
    }
```

In `sandbox/src/macos_seatbelt/tests.rs`, add (mirroring the existing `Net::Allowlist` → `(allow network*)` test around line 135):

```rust
    #[test]
    fn proxy_egress_emits_allow_network() {
        let mut p = SandboxPolicy::default();
        p.net = crate::Net::ProxyEgress;
        let prof = build_profile(&p);
        assert!(prof.contains("(allow network*)"), "ProxyEgress must allow network; got:\n{prof}");
    }
```

In `sandbox/src/macos_container/tests.rs`, add (mirroring the `Net::Allowlist` argv test around line 108):

```rust
    #[test]
    fn proxy_egress_maps_to_network_default() {
        let policy = SandboxPolicy { net: Net::ProxyEgress, ../* reuse the file's existing fixture base */ Default::default() };
        let argv = build_container_argv(/* same args the sibling Allowlist test uses */ &policy, "img", "/bin/true", &[]);
        let s = argv.join(" ");
        assert!(s.contains("--network default"), "got: {s}");
    }
```

> **Note:** match the exact `build_container_argv` signature and fixture style already used by the neighbouring `Net::Allowlist` test in that file (read it first — it constructs the policy and passes image/program/args in a specific way). Keep the new test byte-consistent with its sibling.

- [ ] **Step 6: Build + test the sandbox crate on macOS**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox`
Expected: PASS — the macOS Seatbelt + container builder tests, including the two new ones.

- [ ] **Step 7: Cross-clippy the Linux-gated bwrap arm (Mac-side pre-CI check)**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: exit 0 (sandbox is pure-Rust — this verifies the `linux_bwrap` arm compiles for Linux without a linker; see the memory note on cross-clippy).

- [ ] **Step 8: Commit**

```bash
git add sandbox/src/lib.rs sandbox/src/linux_bwrap.rs sandbox/src/macos_seatbelt.rs sandbox/src/macos_seatbelt/tests.rs sandbox/src/macos_container.rs sandbox/src/macos_container/tests.rs
git commit -m "feat(sandbox): Net::ProxyEgress variant across bwrap/seatbelt/container (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Pure `decision_to_audit` mapping (`core/src/egress/audit.rs`)

**Files:**
- Create: `core/src/egress/mod.rs`
- Create: `core/src/egress/audit.rs`
- Modify: `core/src/lib.rs` (`pub mod egress;`)

- [ ] **Step 1: Add the module to `core/src/lib.rs`**

Find the module declaration block and add (alphabetically near the other `pub mod`s):

```rust
pub mod egress;
```

- [ ] **Step 2: Write `core/src/egress/mod.rs`**

```rust
//! Host-side egress-proxy integration (slice #1).
//!
//! Two responsibilities, both reusable and **not yet wired into `tool_host`**
//! (that hookup lands in slice #2 with force-routing):
//!   - [`audit`]: map a proxy stdout decision line to an audit row (pure).
//!   - [`spawn`]: spawn the sandboxed sidecar proxy on a per-worker UDS.
//!
//! The proxy never touches Postgres (core-only-DB invariant); decisions flow
//! proxy → core stdout-ingest → PG.

pub mod audit;
pub mod spawn;
```

- [ ] **Step 3: Write the failing tests + impl in `core/src/egress/audit.rs`**

```rust
//! Pure mapping from one egress-proxy decision JSON line to an audit row.
//! Keeps the proxy's wire format (snake_case verdicts) as the single source of
//! truth; the DB insert (PG-gated) lives in the e2e, not here.

use serde::Deserialize;

/// Canonical audit actor for egress-proxy decisions.
pub const ACTOR: &str = "egress_proxy";

/// The shape one proxy stdout line deserializes into. Mirrors
/// `egress-proxy::report::Decision`.
#[derive(Debug, Deserialize)]
struct DecisionLine {
    worker: String,
    host: String,
    port: u16,
    resolved_ip: Option<String>,
    verdict: String,
    reason: String,
}

/// An audit row ready for `kastellan_db::audit::insert` (actor + action + payload).
#[derive(Debug, PartialEq, Eq)]
pub struct EgressAuditRow {
    pub actor: &'static str,
    pub action: String,
    pub payload: serde_json::Value,
}

/// Parse one decision line into an audit row. Returns `None` for a line that
/// isn't valid decision JSON (logged-and-skipped by the caller — never trusted
/// to widen anything).
pub fn decision_to_audit(line: &str) -> Option<EgressAuditRow> {
    let d: DecisionLine = serde_json::from_str(line.trim()).ok()?;
    let action = match d.verdict.as_str() {
        "allowed" => "egress.allowed",
        "blocked_allowlist" => "egress.blocked.allowlist",
        "blocked_ssrf" => "egress.blocked.ssrf",
        _ => return None,
    };
    Some(EgressAuditRow {
        actor: ACTOR,
        action: action.to_string(),
        payload: serde_json::json!({
            "worker": d.worker,
            "host": d.host,
            "port": d.port,
            "resolved_ip": d.resolved_ip,
            "reason": d.reason,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_maps_to_action() {
        let line = r#"{"worker":"web-fetch","host":"a.example.com","port":443,"resolved_ip":"203.0.113.5","verdict":"allowed","reason":"ok"}"#;
        let row = decision_to_audit(line).unwrap();
        assert_eq!(row.actor, "egress_proxy");
        assert_eq!(row.action, "egress.allowed");
        assert_eq!(row.payload["host"], "a.example.com");
        assert_eq!(row.payload["resolved_ip"], "203.0.113.5");
    }

    #[test]
    fn blocked_verdicts_map() {
        let mk = |v: &str| format!(r#"{{"worker":"w","host":"h","port":443,"resolved_ip":null,"verdict":"{v}","reason":"r"}}"#);
        assert_eq!(decision_to_audit(&mk("blocked_allowlist")).unwrap().action, "egress.blocked.allowlist");
        assert_eq!(decision_to_audit(&mk("blocked_ssrf")).unwrap().action, "egress.blocked.ssrf");
    }

    #[test]
    fn garbage_line_is_none() {
        assert!(decision_to_audit("not json").is_none());
        assert!(decision_to_audit(r#"{"verdict":"wat"}"#).is_none());
    }
}
```

> Note: `core/src/egress/spawn.rs` is referenced by `mod.rs` but written in Task 9. To keep this task's build green, create a minimal placeholder `core/src/egress/spawn.rs` now containing only `//! (filled in by Task 9)` plus an empty line — or fold Step 1–2 of Task 9 forward. Simplest: create the placeholder file here.

- [ ] **Step 4: Create the spawn.rs placeholder so the module compiles**

Create `core/src/egress/spawn.rs`:

```rust
//! Sidecar spawn — implemented in Task 9.
```

- [ ] **Step 5: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::audit`
Expected: PASS — 3 tests.

- [ ] **Step 6: Commit**

```bash
git add core/src/lib.rs core/src/egress/mod.rs core/src/egress/audit.rs core/src/egress/spawn.rs
git commit -m "feat(core/egress): pure decision_to_audit mapping (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: `spawn_sidecar` + `SidecarHandle` (`core/src/egress/spawn.rs`)

Builds the `Net::ProxyEgress` policy, spawns the sandboxed proxy via the `SandboxBackend`, and waits (bounded) for the UDS to appear. **Not called by `tool_host` this slice** — exercised by the Task 10 e2e.

**Files:**
- Modify: `core/src/egress/spawn.rs`

- [ ] **Step 1: Write `spawn_sidecar` + `SidecarHandle`**

```rust
//! Spawn the sandboxed egress-proxy sidecar on a per-worker UDS and wait for it
//! to be ready. Reusable host-side API; slice #2 calls this from the net-worker
//! bring-up path and ties `SidecarHandle::shutdown` to worker-terminal teardown.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

/// Env keys the sidecar binary reads (must match `egress-proxy::main`).
const ENV_UDS: &str = "KASTELLAN_EGRESS_PROXY_UDS";
const ENV_ALLOWLIST: &str = "KASTELLAN_EGRESS_PROXY_ALLOWLIST";
const ENV_WORKER: &str = "KASTELLAN_EGRESS_PROXY_WORKER";

/// How long `spawn_sidecar` waits for the proxy to `bind()` its UDS.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(25);

/// A running sidecar. Drop or `shutdown()` kills it.
#[derive(Debug)]
pub struct SidecarHandle {
    child: Child,
    pub uds_path: PathBuf,
}

impl SidecarHandle {
    /// Kill the sidecar and reap it. Idempotent-ish (errors ignored).
    pub fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.uds_path);
    }

    /// Borrow the child's stdout for the caller's decision-ingest loop.
    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }
}

/// Build the sandbox policy for the proxy: `Net::ProxyEgress` (real outbound +
/// DNS, self-enforcing), `WorkerNetClient` (permits `socket(2)`), fs_read for
/// the DNS resolver files + the binary, fs_write for the scratch dir (to create
/// the UDS), and the env contract.
pub fn proxy_policy(binary: &Path, allowlist: &[String], scratch: &Path, worker: &str) -> SandboxPolicy {
    let uds = scratch.join("egress.sock");
    let allow_json = serde_json::to_string(allowlist).expect("Vec<String> serializes");
    SandboxPolicy {
        fs_read: vec![
            binary.to_path_buf(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![scratch.to_path_buf()],
        net: Net::ProxyEgress,
        cpu_ms: 10_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![
            (ENV_UDS.to_string(), uds.to_string_lossy().into_owned()),
            (ENV_ALLOWLIST.to_string(), allow_json),
            (ENV_WORKER.to_string(), worker.to_string()),
        ],
    }
}

/// Spawn the proxy under `backend` and wait (bounded) for its UDS to appear.
/// Fail-closed: returns `Err` on spawn failure or bind timeout.
pub fn spawn_sidecar(
    backend: &dyn SandboxBackend,
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
) -> anyhow::Result<SidecarHandle> {
    let policy = proxy_policy(binary, allowlist, scratch, worker);
    let uds_path = scratch.join("egress.sock");
    let _ = std::fs::remove_file(&uds_path);

    let program = binary.to_string_lossy();
    let child = backend
        .spawn_under_policy(&policy, &program, &[])
        .map_err(|e| anyhow::anyhow!("spawn egress-proxy sidecar: {e}"))?;

    let deadline = Instant::now() + READY_TIMEOUT;
    while !uds_path.exists() {
        if Instant::now() >= deadline {
            let mut handle = SidecarHandle { child, uds_path: uds_path.clone() };
            handle.child.kill().ok();
            handle.child.wait().ok();
            anyhow::bail!("egress-proxy sidecar did not bind {uds_path:?} within {READY_TIMEOUT:?}");
        }
        std::thread::sleep(READY_POLL);
    }
    Ok(SidecarHandle { child, uds_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_uses_proxy_egress_and_net_client() {
        let p = proxy_policy(Path::new("/opt/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch");
        assert!(matches!(p.net, Net::ProxyEgress));
        assert!(matches!(p.profile, Profile::WorkerNetClient));
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        assert!(p.fs_write.contains(&PathBuf::from("/scratch")));
        // env carries the UDS path + allowlist + worker name.
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_UDS], "/scratch/egress.sock");
        assert_eq!(env[ENV_ALLOWLIST], r#"["example.com"]"#);
        assert_eq!(env[ENV_WORKER], "web-fetch");
    }
}
```

- [ ] **Step 2: Run the unit test**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::spawn`
Expected: PASS — 1 test (`policy_uses_proxy_egress_and_net_client`).

- [ ] **Step 3: Build + clippy core**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core && cargo clippy -p kastellan-core --all-targets -- -D warnings`
Expected: builds clean, clippy exit 0.

- [ ] **Step 4: Commit**

```bash
git add core/src/egress/spawn.rs
git commit -m "feat(core/egress): spawn_sidecar + SidecarHandle (ProxyEgress policy, bounded UDS wait) (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: End-to-end test (`core/tests/egress_proxy_e2e.rs`)

Spawns the **real sandboxed** sidecar, drives it with a test CONNECT client over the UDS, and asserts allowlist + SSRF + audit-mapping end-to-end. Skip-as-pass when the sandbox/binary is unavailable (macOS posture). Plus an `#[ignore]` real-network test and a PG-gated audit-insert.

**Files:**
- Create: `core/tests/egress_proxy_e2e.rs`

- [ ] **Step 1: Write the e2e (hermetic + ignored real-net)**

```rust
//! End-to-end: `core::egress::spawn_sidecar` brings up the egress-proxy under
//! the real platform sandbox; a test CONNECT client over the UDS exercises the
//! allowed / blocked paths; `decision_to_audit` maps the proxy's stdout stream.
//!
//! Hermetic test drives a localhost origin via a literal-allowlisted CONNECT.
//! `[SKIP]`s cleanly when the sandbox or the worker binary is missing — same
//! posture as `web_fetch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;

use kastellan_core::egress::audit::decision_to_audit;
use kastellan_core::egress::spawn::spawn_sidecar;
use kastellan_tests_common::{
    backend, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};

/// Locate the built proxy binary; `[SKIP]` if absent.
fn proxy_binary_or_skip() -> Option<std::path::PathBuf> {
    workspace_target_binary("kastellan-worker-egress-proxy")
}

#[test]
fn allowed_literal_origin_round_trips_and_blocks_off_allowlist() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(binary) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };

    // A localhost origin that echoes a token after the client writes.
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_thread = std::thread::spawn(move || {
        if let Ok((mut s, _)) = origin.accept() {
            let mut buf = [0u8; 8];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"PONG");
        }
    });

    // Scratch dir (must be writable by the sandboxed proxy to create the UDS).
    let scratch = std::env::temp_dir().join(format!("egress-e2e-{}", unique_suffix()));
    std::fs::create_dir_all(&scratch).unwrap();

    // Allowlist: the literal loopback origin (the local-SearxNG carve-out shape).
    let allowlist = vec!["127.0.0.1".to_string()];
    let backend = backend();
    let mut handle = spawn_sidecar(backend.as_ref(), &binary, &allowlist, &scratch, "web-fetch")
        .expect("sidecar spawns and binds UDS");
    let stdout = handle.stdout().expect("child stdout piped");

    // Allowed round-trip via CONNECT to the literal-allowlisted origin.
    let mut client = UnixStream::connect(&handle.uds_path).unwrap();
    write!(client, "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").unwrap();
    let mut head = [0u8; 39];
    client.read_exact(&mut head).unwrap();
    assert!(std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"));
    client.write_all(b"ping").unwrap();
    let mut echo = [0u8; 4];
    client.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"PONG");
    drop(client);
    origin_thread.join().unwrap();

    // Off-allowlist CONNECT is blocked at the boundary.
    let mut bad = UnixStream::connect(&handle.uds_path).unwrap();
    write!(bad, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    let _ = bad.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 403"), "got {resp:?}");

    // Drain the decision stream and map to audit rows.
    let reader = BufReader::new(stdout);
    let mut actions = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        if let Some(row) = decision_to_audit(&line) {
            actions.push(row.action);
        }
        if actions.len() >= 2 {
            break;
        }
    }
    handle.shutdown();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(actions.contains(&"egress.allowed".to_string()), "actions: {actions:?}");
    assert!(actions.contains(&"egress.blocked.allowlist".to_string()), "actions: {actions:?}");
}

/// Real-network: a test CONNECT to a real public host round-trips through the
/// sandboxed proxy (validates DNS + IP-pinning + tunnel + TLS-in-jail end to
/// end). Run with `--ignored` and network access.
#[test]
#[ignore = "real network: validates DNS + pinning + tunnel through the sandboxed proxy"]
fn real_host_round_trips_through_sidecar() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(binary) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };
    let scratch = std::env::temp_dir().join(format!("egress-e2e-real-{}", unique_suffix()));
    std::fs::create_dir_all(&scratch).unwrap();
    let allowlist = vec!["example.com".to_string()];
    let backend = backend();
    let mut handle = spawn_sidecar(backend.as_ref(), &binary, &allowlist, &scratch, "web-fetch")
        .expect("sidecar spawns");

    let mut client = UnixStream::connect(&handle.uds_path).unwrap();
    write!(client, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut head = [0u8; 39];
    client.read_exact(&mut head).unwrap();
    assert!(std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"),
            "expected a tunnel to a real allowlisted public host");

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&scratch);
}
```

> **Verification notes for the implementer:**
> - Confirm `kastellan_tests_common` actually exports `workspace_target_binary` and `backend` (it does per `tests-common/src/lib.rs`; if the helper name differs, grep `tests-common/src/` and match it).
> - **macOS (dev box):** the e2e runs the proxy under Seatbelt; the scratch dir is `fs_write`, so the UDS is created at the literal path and the host test client connects directly. Landlock/seccomp are no-ops here, so the server-socket seccomp question does **not** gate this run.
> - **Linux (DGX/CI):** verify (a) the bind-mounted scratch path is identical host-side and in-jail so the test client can reach the UDS, and (b) the `net_client` seccomp profile permits AF_UNIX `accept` (see the note in Task 6 / main.rs). If `accept` is killed, widen `workers/prelude/src/seccomp_lock.rs` in a focused follow-up commit and pin it with a `prelude` smoke test.

- [ ] **Step 2: Build the proxy binary so the e2e can find it, then run the e2e**

Run:
```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-worker-egress-proxy
cargo test -p kastellan-core --test egress_proxy_e2e
```
Expected: PASS on macOS — `allowed_literal_origin_round_trips_and_blocks_off_allowlist` passes; the `#[ignore]` test is listed but not run.

- [ ] **Step 3: (Optional, when on a network) run the ignored real-net test**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test egress_proxy_e2e -- --ignored --nocapture`
Expected: PASS (tunnels to `example.com:443`).

- [ ] **Step 4: Commit**

```bash
git add core/tests/egress_proxy_e2e.rs
git commit -m "test(egress-proxy): e2e — sandboxed sidecar + test CONNECT client, allow/block/audit (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: PG-gated audit-insert test + workspace verification

Proves a decision row actually lands in `audit_log` (skip-as-pass without PG on the Mac), then runs the full workspace.

**Files:**
- Modify: `core/tests/egress_proxy_e2e.rs` (add one PG-gated test)

- [ ] **Step 1: Add a PG-gated audit-insert test**

Append to `core/tests/egress_proxy_e2e.rs` (model the PG bring-up on `web_fetch_e2e.rs`'s `bring_up_pg_cluster` + `pg_bin_dir_or_skip` idiom — read it for the exact helper signatures). The test: build an `EgressAuditRow` from a sample decision line via `decision_to_audit`, insert it with `kastellan_db::audit::insert`, and read it back asserting `actor='egress_proxy'` and the `action`.

```rust
#[test]
fn decision_row_persists_to_audit_log() {
    // PG-gated: [SKIP] without a usable PG bin dir (macOS skip-as-pass posture).
    let Some(_pg_bin) = kastellan_tests_common::pg_bin_dir_or_skip() else { return };
    // ... bring up the cluster (see web_fetch_e2e.rs probe_and_pool), then:
    //   let row = decision_to_audit(SAMPLE_ALLOWED_LINE).unwrap();
    //   kastellan_db::audit::insert(&pool, row.actor, &row.action, &row.payload).await?;
    //   read back the latest row and assert actor + action.
    // Use the exact kastellan_db::audit::insert signature (grep db/src/audit.rs).
}
```

> **Implementer:** fill the body using the real `kastellan_db::audit::insert` signature and the `web_fetch_e2e.rs` PG harness. Keep it skip-as-pass: a missing PG bin dir returns early. Do not block the macOS workspace run on PG.

- [ ] **Step 2: Run the full workspace test suite (macOS skip-as-pass)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace`
Expected: all green; `[SKIP]` lines only for PG/sandbox/GLiNER-gated suites. Record the passed/failed/ignored counts for the handover.

- [ ] **Step 3: Workspace clippy gate**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add core/tests/egress_proxy_e2e.rs
git commit -m "test(egress-proxy): PG-gated audit_log persistence for egress decisions (ROADMAP:141)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Update HANDOVER + ROADMAP, threat-model note

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`
- Modify: `docs/threat-model.md`

- [ ] **Step 1: ROADMAP** — under Phase 3, mark the egress-proxy line as partially done: add a `[x]` sub-bullet noting **slice #1 (boundary allowlist + SSRF/IP defense) shipped** with the commit range, and that force-routing / leak-scanner / TLS-pinning remain (slices #2–4). Keep the parent `- [ ] Egress proxy` unchecked (the full subsystem isn't done).

- [ ] **Step 2: threat-model.md** — in the "Network egress" section, update the SSRF/DNS-rebinding paragraph: the gap is now **closed by the egress proxy for traffic that routes through it** (resolve-then-pin + range deny), while noting real workers don't route through it until slice #2's force-routing. Update the "Egress proxy" defence-in-depth row if needed.

- [ ] **Step 3: HANDOVER.md** — per the doc's own "How to update" checklist: bump `Last updated`, `Current state` (new commit hash), add a "Recently completed — egress proxy slice #1" section (file paths, the `Net::ProxyEgress` change, what's deferred to #2), refresh "Working state" (new `egress-proxy` crate + `core/src/egress` + the 13th workspace crate), and write a fresh "Next TODO" whose top pick is **egress-proxy slice #2** (force-routing: private netns + the `web-common` CONNECT-over-UDS hyper client + the `tool_host` auto-spawn hookup). Copy the session test counts into `Session-end verification`.

- [ ] **Step 4: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/threat-model.md
git commit -m "docs(handover): egress-proxy slice #1 shipped; ROADMAP:141 partial; threat-model SSRF update

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open PR**

```bash
git push -u origin feat/egress-proxy-boundary
gh pr create --base main --title "feat: egress proxy slice #1 — boundary allowlist + SSRF/IP defense (ROADMAP:141)" --body "$(cat <<'EOF'
## Summary
Slice #1 of the egress proxy (ROADMAP:141): a sandboxed per-worker proxy that enforces the host allowlist at the boundary and closes the SSRF/DNS-rebinding gap (resolve-then-pin, allowlist-aware private-IP deny). Proven end-to-end by a test CONNECT client. Does **not** route real workers through it yet — that, plus force-routing, lands in slice #2.

## What shipped
- New `workers/egress-proxy` crate: `ssrf` (range classifier) + `request_line` (CONNECT parse) + `report` (decision JSON) + `proxy` (drive loop, allowlist + SSRF + IP-pinning + literal-IP carve-out) + `main` (UDS bind + lockdown + accept loop).
- `Net::ProxyEgress` sandbox variant across bwrap / Seatbelt / container.
- `core/src/egress`: pure `decision_to_audit` + `spawn_sidecar`/`SidecarHandle` (not yet wired into `tool_host`).
- e2e: real sandboxed sidecar + test CONNECT client (allow/block/audit) + `#[ignore]` real-net + PG-gated audit insert.

## Deferred (own specs)
Slice #2 force-routing (netns + web-common CONNECT-over-UDS client + `tool_host` hookup); slice #3 TLS-intercept leak scanner; slice #4 TLS pinning.

Design: `docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review (completed during planning)

- **Spec coverage:** every Goal maps to a task — proxy binary (T1–6), allowlist enforcement (T5 `decide`), SSRF resolve-then-pin (T2 + T5), audit without DB-in-proxy (T4 + T8), sandboxed proxy / `Net::ProxyEgress` (T7 + T9), reusable `core/src/egress` + e2e via test client (T8–11). Deferred items (worker transport, force-routing, TLS) are explicitly out of scope per the spec.
- **Placeholder scan:** the only deliberate stubs are the Task 8 `spawn.rs` placeholder (filled in T9) and the two test-body sketches (T7 container test, T11 PG test) that point at an existing sibling to copy exactly — flagged with read-the-sibling notes because their precise signatures live in files the implementer must match rather than ones invented here.
- **Type consistency:** `Decision`/`Verdict` (snake_case wire form) are defined in T4 and consumed verbatim by `decision_to_audit` in T8; `Net::ProxyEgress` defined in T7 and used in T9's `proxy_policy`; `spawn_sidecar`/`SidecarHandle` defined in T9 and used in T10. The proxy env keys (`KASTELLAN_EGRESS_PROXY_{UDS,ALLOWLIST,WORKER}`) match between `main.rs` (T6) and `spawn.rs` (T9).
