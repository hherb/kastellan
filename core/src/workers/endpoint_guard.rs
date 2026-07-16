//! Resolve-time endpoint guard for force-routed workers (#452 / #429).
//!
//! When a `Net::Allowlist` worker's egress is force-routed through the host
//! egress proxy, the proxy's SSRF/DNS-rebinding defense range-checks every
//! **resolved hostname** — but an operator-allowlisted **literal IP** is
//! dialed with the range check deliberately skipped (the allowlisted-literal
//! carve-out in `egress-proxy::proxy::decide`; operator intent is explicit and
//! a literal cannot be rebound). Both guarded workers derive their allowlist
//! from the endpoint itself, so a literal endpoint — loopback included — is
//! REACHABLE when force-routed.
//!
//! The one endpoint class that is statically knowable to be dead is an RFC
//! 6761 `localhost` / `*.localhost` **name**: it takes the proxy's hostname
//! path, always resolves to loopback, and is range-denied on every CONNECT.
//! The worker then registers and looks healthy while every request fails — a
//! silent footgun (#452). The helpers here let a manifest refuse that dead
//! configuration at `resolve()` time (`Resolution::Misconfigured`), or — for
//! web-research's *optional* embed endpoint, where the same class only
//! degrades ranking — warn (#429).
//!
//! Deliberately **no DNS at resolve time**: any other hostname that happens to
//! resolve to a private address (including a rebinding attack) is caught by
//! the authoritative connect-time proxy check. These helpers classify only
//! what is knowable statically.

use url::{Host, Url};

use crate::worker_lifecycle::force_route;

/// True iff `endpoint`'s host is an RFC 6761 `localhost` / `*.localhost`
/// name — the one endpoint class a force-routed worker can never reach (the
/// proxy resolves the name, gets loopback, and range-denies the CONNECT).
///
/// Literal IPs — loopback/private included — return `false`: the proxy's
/// allowlisted-literal carve-out dials them, so they work when force-routed.
/// A real remote hostname also returns `false` (resolve-time cannot know its
/// address without DNS; the connect-time proxy check owns that case), and so
/// does an unset / unparseable endpoint (those keep today's fail-closed worker
/// startup behaviour instead of a guard message about the wrong problem).
pub(crate) fn endpoint_is_localhost_name(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else { return false };
    match url.host() {
        Some(Host::Domain(d)) => is_local_domain(d),
        // Ipv4/Ipv6 literals: reachable via the proxy's carve-out; no host: no
        // guard business either way.
        _ => false,
    }
}

/// RFC 6761: `localhost` and any `*.localhost` name always resolve to loopback.
fn is_local_domain(domain: &str) -> bool {
    let d = domain.trim_end_matches('.'); // tolerate a FQDN trailing dot
    d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
}

/// True iff a `Net::Allowlist` worker spawned by this daemon will have its
/// egress force-routed through the host egress proxy: always in micro-VM mode
/// (`linux_firecracker/plan.rs` refuses to give a `Net::Allowlist` VM worker a
/// NIC), and in host mode iff the operator enabled
/// `KASTELLAN_EGRESS_FORCE_ROUTING`. The flag name and truthiness parse are
/// `force_route`'s own (`ENV_ENABLE` / `env_flag_enabled`), so this mirror
/// cannot drift from the spawn path.
pub(crate) fn egress_will_force_route(
    is_microvm: bool,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    is_microvm || force_route::env_flag_enabled(get_env(force_route::ENV_ENABLE))
}

/// `Some(warning)` iff web-research's *optional* embed endpoint is configured
/// but unreachable: egress is force-routed, the embed-broker is not enabled,
/// and the endpoint is a `localhost` name (the proxy range-denies what it
/// resolves to). The worker still functions — ranking silently degrades
/// hybrid→lexical — so this is an operator warning, not `Misconfigured`
/// (#429). `None` when not force-routed (the worker resolves `localhost`
/// itself), when brokered (the embed-broker reaches the backend over its
/// UDS), or when the endpoint is a literal IP (reachable via the proxy's
/// allowlisted-literal carve-out) / routable / unset.
pub(crate) fn embed_local_warning(
    force_routed: bool,
    use_broker: bool,
    embed_endpoint: Option<&str>,
) -> Option<String> {
    if !force_routed || use_broker {
        return None;
    }
    let embed = embed_endpoint?;
    if !endpoint_is_localhost_name(embed) {
        return None;
    }
    Some(format!(
        "web-research: KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT ({embed}) uses a \
         `localhost` name while egress is force-routed: the egress proxy refuses \
         to resolve localhost names (SSRF/DNS-rebinding defense), so the query \
         embed will fail and ranking degrades hybrid->lexical. Use the literal-IP \
         form (e.g. http://127.0.0.1:11434 — an allowlisted literal is dialed via \
         the proxy's carve-out), a routable host, or set \
         KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_literals_are_not_flagged_even_when_loopback_or_private() {
        // The egress proxy's operator-allowlisted-literal carve-out
        // (`egress-proxy::proxy::decide`) dials an allowlisted literal IP with
        // the SSRF range check skipped, and both workers derive their
        // allowlist from the endpoint — so a literal endpoint is REACHABLE
        // when force-routed and must never be flagged as dead.
        for ep in [
            "http://127.0.0.1:8888/search",
            "https://127.1.2.3/",
            "http://[::1]:8888/search",
            "http://10.0.0.5:8080/",
            "http://172.16.5.5/",
            "http://192.168.1.1:11434/v1/embeddings",
            "http://169.254.1.1/",
            "http://100.64.0.1/", // CGNAT
            "http://[fd12:3456::1]/", // ULA
        ] {
            assert!(!endpoint_is_localhost_name(ep), "{ep} must not be flagged (carve-out)");
        }
    }

    #[test]
    fn localhost_names_are_flagged() {
        // RFC 6761 names take the proxy's HOSTNAME path: resolve → loopback →
        // range-denied → BlockedSsrf. The one statically-knowable dead class.
        assert!(endpoint_is_localhost_name("http://localhost:8888/search"));
        assert!(endpoint_is_localhost_name("http://LOCALHOST/"));
        assert!(endpoint_is_localhost_name("https://searx.localhost/search"));
        assert!(endpoint_is_localhost_name("http://localhost./")); // FQDN trailing dot
    }

    #[test]
    fn public_hosts_and_ips_are_not_flagged() {
        assert!(!endpoint_is_localhost_name("https://searx.example.org/search"));
        assert!(!endpoint_is_localhost_name("https://searx.example.org:8888/search"));
        assert!(!endpoint_is_localhost_name("http://203.0.113.5:8888/"));
        assert!(!endpoint_is_localhost_name("http://[2606:4700:4700::1111]/"));
    }

    #[test]
    fn rebinding_lookalike_domain_is_not_flagged() {
        // A hostname merely *containing* "localhost" is still an ordinary
        // domain — what it resolves to is the connect-time proxy's job.
        assert!(!endpoint_is_localhost_name("http://127.0.0.1.attacker.example/"));
        assert!(!endpoint_is_localhost_name("http://localhost.attacker.example/"));
    }

    #[test]
    fn unset_or_unparseable_endpoints_are_not_flagged() {
        assert!(!endpoint_is_localhost_name(""));
        assert!(!endpoint_is_localhost_name("not a url"));
        assert!(!endpoint_is_localhost_name("127.0.0.1:8888/search")); // no scheme
    }

    #[test]
    fn microvm_always_force_routes_regardless_of_flag() {
        let unset = |_k: &str| None;
        assert!(egress_will_force_route(true, &unset));
        let off = |_k: &str| Some("0".to_string());
        assert!(egress_will_force_route(true, &off));
    }

    #[test]
    fn host_mode_follows_the_force_routing_flag_truthiness() {
        // Mirrors force_route::env_flag_enabled: 1|true|yes|on, trimmed,
        // case-insensitive.
        for v in ["1", "true", "yes", "on", " TRUE "] {
            let on =
                move |k: &str| (k == "KASTELLAN_EGRESS_FORCE_ROUTING").then(|| v.to_string());
            assert!(egress_will_force_route(false, &on), "{v:?} should enable");
        }
        for v in ["0", "false", "off", "", "banana"] {
            let off =
                move |k: &str| (k == "KASTELLAN_EGRESS_FORCE_ROUTING").then(|| v.to_string());
            assert!(!egress_will_force_route(false, &off), "{v:?} must not enable");
        }
        let unset = |_k: &str| None;
        assert!(!egress_will_force_route(false, &unset));
    }

    #[test]
    fn warns_only_when_forced_unbrokered_and_localhost_name() {
        let localhost_name = Some("http://localhost:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, localhost_name).is_some());
        // Not force-routed: the worker resolves localhost itself, no proxy.
        assert!(embed_local_warning(false, false, localhost_name).is_none());
        // Brokered: the embed-broker reaches the backend over its UDS.
        assert!(embed_local_warning(true, true, localhost_name).is_none());
        // A LITERAL loopback embed endpoint is reachable via the proxy's
        // allowlisted-literal carve-out (it is unioned into net_entries) —
        // never warn about a working config.
        let literal = Some("http://127.0.0.1:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, literal).is_none());
        // Routable or unset endpoint: nothing to warn about.
        let routable = Some("http://embed.example.org:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, routable).is_none());
        assert!(embed_local_warning(true, false, None).is_none());
    }

    #[test]
    fn warning_names_the_env_and_the_remedies() {
        let w = embed_local_warning(true, false, Some("http://localhost:11434/v1/embeddings"))
            .expect("should warn");
        assert!(w.contains("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"), "warning: {w}");
        assert!(w.contains("KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1"), "warning: {w}");
        assert!(w.contains("127.0.0.1"), "literal-IP remedy missing: {w}");
    }
}
