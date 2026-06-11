//! The per-connection drive loop: parse CONNECT, enforce allowlist + SSRF,
//! pin the resolved IP, and tunnel. Pure decision logic is separated from I/O
//! so the policy paths are unit-testable; `handle_conn` does the byte-shuffling.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::time::Duration;

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

/// Bound on the upstream TCP connect so a slow/unreachable pinned IP cannot pin
/// a proxy thread open indefinitely. Resolution (`to_socket_addrs`) has no std
/// timeout, and the per-direction tunnel copy is deliberately *not* idle-capped
/// in slice #1 (a sane idle timeout depends on the live worker's workload —
/// deferred to slice #2 when real traffic flows; tracked in #242). The sidecar
/// is `SingleUse` and dies with worker teardown, which bounds the worst case.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Decide what to do with `host:port`, given the allowlist and a resolver.
/// - Literal-IP target: allowed iff the allowlist accepts the literal `host:port`
///   endpoint; the SSRF range check is **skipped** (operator allowlisted that
///   exact addr).
/// - Hostname target: allowed iff the allowlist accepts the `host:port`
///   endpoint; then every resolved IP is range-checked and the first non-denied
///   one is pinned.
///
/// **Port scope (#241):** the allowlist matches the `host:port` *endpoint* — an
/// allowlisted host is reachable only on its declared port(s). A bare-host
/// entry (no `:port`) still grants any port (the weaker, back-compat form); when
/// the match succeeds via such an entry, the allowed decision's reason is
/// flagged via [`allowed_reason`] so a port-unconstrained grant is visible in
/// the audit trail rather than silent.
pub fn decide(host: &str, port: u16, allow: &HostAllowlist, resolver: &dyn Resolve) -> Target {
    if !allow.is_allowed_endpoint(host, port) {
        return Target::Block(
            Verdict::BlockedAllowlist,
            format!("{host}:{port} not on allowlist"),
        );
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

/// The audit reason for an allowed CONNECT. When `host` was permitted by a
/// port-scoped entry the reason is the plain `"ok"`; when it was permitted only
/// by a bare host-only (port-unconstrained) entry the reason is the distinct
/// marker `"allowed:host-only-entry"`, so the weaker grant is visible in
/// `audit_log` rather than indistinguishable from a port-scoped allow.
pub fn allowed_reason(allow: &HostAllowlist, host: &str) -> &'static str {
    if allow.is_port_scoped(host) {
        "ok"
    } else {
        "allowed:host-only-entry"
    }
}

/// The TLS-interception context threaded into `handle_conn`: the per-instance CA
/// (signs leaves), the per-connection leaf cache, and the upstream trust config
/// (the REAL origin roots — webpki in production). Bundled so the handler arg
/// count stays sane.
pub struct MitmCtx<'a> {
    pub ca: &'a crate::ca::CaMaterial,
    pub leaf_cache: &'a mut crate::leaf_cache::LeafCache,
    pub upstream_tls: std::sync::Arc<rustls::ClientConfig>,
}

/// Handle one accepted UDS connection end-to-end. Reads the CONNECT line,
/// decides, and on `Dial` pins the IP, replies 200, peeks the first tunnel byte,
/// then branches: MITM-terminate (TLS ClientHello) or plaintext pass-through.
/// Always emits exactly one decision to `reporter` (transport failures after an
/// allow may emit an additional allowed-but-failed record).
pub fn handle_conn(
    mut client: UnixStream,
    worker: &str,
    allow: &HostAllowlist,
    resolver: &dyn Resolve,
    reporter: &mut dyn Reporter,
    mitm: &mut MitmCtx<'_>,
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
            // Connect upstream FIRST (preserves the 502-on-connect-fail behaviour
            // and the pinned-IP SSRF guarantee), THEN reply 200, THEN peek.
            let upstream = match TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_TIMEOUT) {
                Ok(s) => s,
                Err(e) => {
                    // Transport failure, not a policy verdict: decision was allowed.
                    reporter.report(Decision {
                        worker: worker.into(), host: host.clone(), port,
                        resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                        reason: format!("connect_failed: {e}"), tls_intercepted: false,
                    });
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                    return;
                }
            };
            if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").is_err() {
                return;
            }
            // Peek the first tunnel byte (non-consuming). The CONNECT round-trip
            // guarantees the worker only sends after the 200, so this is the
            // first tunnel byte. EOF / error → treat as pass-through.
            let is_tls = peek_first_byte(&client)
                .map(crate::mitm::looks_like_tls)
                .unwrap_or(false);
            if is_tls {
                // MITM branch: the sync `upstream` proved reachability + the 502
                // path; `intercept` re-dials a tokio stream itself, so drop it.
                let _ = upstream;
                reporter.report(Decision {
                    worker: worker.into(), host: host.clone(), port,
                    resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                    reason: allowed_reason(allow, &host).into(), tls_intercepted: true,
                });
                run_mitm(client, ip, port, &host, mitm, worker, reporter);
            } else {
                reporter.report(Decision {
                    worker: worker.into(), host: host.clone(), port,
                    resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                    reason: allowed_reason(allow, &host).into(), tls_intercepted: false,
                });
                tunnel(client, upstream);
            }
        }
    }
}

/// Bridge the sync accept path to the async MITM. Builds a per-connection
/// current-thread runtime (mirrors web-common's ProxyConnectGet) and runs
/// `mitm::intercept`. A handshake/copy error is reported as an allowed-but-failed
/// decision (the policy verdict was Allowed; this is a transport failure).
fn run_mitm(
    client: UnixStream,
    ip: IpAddr,
    port: u16,
    host: &str,
    mitm: &mut MitmCtx<'_>,
    worker: &str,
    reporter: &mut dyn Reporter,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            reporter.report(Decision {
                worker: worker.into(), host: host.into(), port,
                resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                reason: format!("mitm_runtime_failed: {e}"), tls_intercepted: true,
            });
            return;
        }
    };
    let upstream_tls = std::sync::Arc::clone(&mitm.upstream_tls);
    let res = rt.block_on(async move {
        // Convert the accepted std UnixStream into a tokio handle inside the
        // runtime (requires nonblocking + an active reactor).
        client.set_nonblocking(true).map_err(|e| format!("client nonblocking: {e}"))?;
        let client = tokio::net::UnixStream::from_std(client)
            .map_err(|e| format!("client from_std: {e}"))?;
        crate::mitm::intercept(
            client,
            SocketAddr::new(ip, port),
            host,
            mitm.ca,
            mitm.leaf_cache,
            upstream_tls,
        )
        .await
    });
    if let Err(e) = res {
        reporter.report(Decision {
            worker: worker.into(), host: host.into(), port,
            resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
            reason: format!("mitm_failed: {e}"), tls_intercepted: true,
        });
    }
}

fn blocked(worker: &str, host: &str, port: u16, verdict: Verdict, reason: &str) -> Decision {
    Decision {
        worker: worker.into(), host: host.into(), port,
        resolved_ip: None, verdict, reason: reason.into(), tls_intercepted: false,
    }
}

/// Hard cap on the bytes we read for the whole CONNECT request head (line +
/// header block). A legitimate CONNECT request is well under this; the cap stops
/// a malicious UDS client from growing the heap with an endless line before the
/// sandbox memory limit would otherwise have to step in (defense-in-depth).
const MAX_REQUEST_HEAD_BYTES: u64 = 8 * 1024;

/// Read just the CONNECT request line (up to the first CRLF), then drain the
/// remaining header block up to the blank line so the tunnel starts clean.
/// The total bytes read are capped at [`MAX_REQUEST_HEAD_BYTES`]; if the line
/// is truncated by the cap it simply fails to parse downstream and is blocked.
fn read_request_line(client: &mut UnixStream) -> std::io::Result<String> {
    // `Read::take` bounds total bytes across every `read_line` below.
    let mut reader = BufReader::new(client.try_clone()?.take(MAX_REQUEST_HEAD_BYTES));
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

/// Non-consuming peek of the first tunnel byte via `recv(MSG_PEEK)` on the raw
/// fd (std's `UnixStream::peek` is still unstable). Blocks until ≥1 byte is
/// available or the peer half-closes. Returns `Some(byte)` on a 1-byte peek,
/// `None` on EOF / error (caller treats that as plaintext pass-through).
fn peek_first_byte(client: &UnixStream) -> Option<u8> {
    use std::os::unix::io::AsRawFd;
    let mut byte = 0u8;
    // SAFETY: `client` owns the fd for the duration of this call; we pass a valid
    // 1-byte buffer and read the return value before trusting `byte`.
    let n = unsafe {
        libc::recv(
            client.as_raw_fd(),
            &mut byte as *mut u8 as *mut libc::c_void,
            1,
            libc::MSG_PEEK,
        )
    };
    if n == 1 {
        Some(byte)
    } else {
        None
    }
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
        // Half-close upstream's write side so the origin sees EOF on the client
        // direction while still draining its own response back to us.
        let _ = uw.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut ur, &mut cw);
    // Upstream closed → tear down both client halves, then reap the up thread.
    // The asymmetry (Write above, Both here) is intentional: the spawned copy
    // can still be blocked reading the client, and `join` waits only as long as
    // the peer keeps its write side open — bounded for a SingleUse worker.
    let _ = cw.shutdown(std::net::Shutdown::Both);
    let _ = up.join();
}

#[cfg(test)]
mod tests;
