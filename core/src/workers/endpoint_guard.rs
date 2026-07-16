//! Resolve-time endpoint locality guard (#452 / #429).
//!
//! When a `Net::Allowlist` worker's egress is force-routed through the host
//! egress proxy, the proxy SSRF-blocks loopback / RFC1918 / CGNAT destinations
//! (the `kastellan-net-classify` range list — the same one the proxy enforces
//! at connect time). An operator endpoint pointing at such an address is
//! therefore unreachable in a force-routed mode: the worker registers and
//! looks healthy, but every request fails — a silent footgun (#452). The
//! helpers here let a manifest detect that dead configuration at `resolve()`
//! time and refuse to register, or — for web-research's *optional* embed
//! endpoint, where a local address only degrades ranking — warn (#429).
//!
//! Deliberately **no DNS at resolve time**: a real hostname that later
//! resolves to loopback (DNS rebinding) is caught by the authoritative
//! connect-time proxy SSRF check. These helpers classify only what is knowable
//! statically — IP literals and the RFC 6761 `localhost` names.

use std::net::IpAddr;

use kastellan_net_classify::is_denied_range;
use url::{Host, Url};

use crate::worker_lifecycle::force_route;

/// True iff `endpoint`'s host is a loopback/private/link-local/CGNAT IP literal
/// or a `localhost` / `*.localhost` name (RFC 6761) — i.e. an address the
/// force-routed egress proxy will refuse to CONNECT to.
///
/// A real remote hostname returns `false` (resolve-time cannot know its address
/// without DNS; the connect-time proxy check owns that case), and so does an
/// unset / unparseable endpoint (those keep today's fail-closed worker startup
/// behaviour instead of a guard message about the wrong problem).
pub(crate) fn endpoint_host_is_local(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else { return false };
    match url.host() {
        Some(Host::Ipv4(a)) => is_denied_range(IpAddr::V4(a)),
        Some(Host::Ipv6(a)) => is_denied_range(IpAddr::V6(a)),
        Some(Host::Domain(d)) => is_local_domain(d),
        None => false,
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
/// but unreachable: egress is force-routed (the proxy SSRF-blocks local
/// addresses), the embed-broker is not enabled, and the endpoint host is
/// local. The worker still functions — ranking silently degrades
/// hybrid→lexical — so this is an operator warning, not `Misconfigured`
/// (#429). `None` when not force-routed (host egress reaches loopback fine),
/// when brokered (the embed-broker reaches the backend over its UDS), or when
/// the endpoint is routable/unset.
pub(crate) fn embed_local_warning(
    force_routed: bool,
    use_broker: bool,
    embed_endpoint: Option<&str>,
) -> Option<String> {
    if !force_routed || use_broker {
        return None;
    }
    let embed = embed_endpoint?;
    if !endpoint_host_is_local(embed) {
        return None;
    }
    Some(format!(
        "web-research: KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT ({embed}) points at a \
         loopback/private host while egress is force-routed (the egress proxy \
         SSRF-blocks it): the query embed will fail and ranking degrades \
         hybrid->lexical. Point it at a routable host or set \
         KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_private_ip_literals_are_local() {
        for ep in [
            "http://127.0.0.1:8888/search",
            "https://127.1.2.3/",
            "http://[::1]:8888/search",
            "http://10.0.0.5:8080/",
            "http://172.16.5.5/",
            "http://192.168.1.1:11434/v1/embeddings",
            "http://169.254.1.1/",
            "http://100.64.0.1/", // CGNAT
            "http://0.0.0.0:9/",
            "http://[fd12:3456::1]/", // ULA
        ] {
            assert!(endpoint_host_is_local(ep), "{ep} should be local");
        }
    }

    #[test]
    fn localhost_names_are_local() {
        assert!(endpoint_host_is_local("http://localhost:8888/search"));
        assert!(endpoint_host_is_local("http://LOCALHOST/"));
        assert!(endpoint_host_is_local("https://searx.localhost/search"));
        assert!(endpoint_host_is_local("http://localhost./")); // FQDN trailing dot
    }

    #[test]
    fn public_hosts_and_ips_are_not_local() {
        assert!(!endpoint_host_is_local("https://searx.example.org/search"));
        assert!(!endpoint_host_is_local("https://searx.example.org:8888/search"));
        assert!(!endpoint_host_is_local("http://203.0.113.5:8888/"));
        assert!(!endpoint_host_is_local("http://[2606:4700:4700::1111]/"));
    }

    #[test]
    fn rebinding_lookalike_domain_is_not_local() {
        // A hostname merely *containing* a loopback string is still a domain —
        // what it resolves to is the connect-time proxy's job, not ours.
        assert!(!endpoint_host_is_local("http://127.0.0.1.attacker.example/"));
        assert!(!endpoint_host_is_local("http://localhost.attacker.example/"));
    }

    #[test]
    fn unset_or_unparseable_endpoints_are_not_local() {
        assert!(!endpoint_host_is_local(""));
        assert!(!endpoint_host_is_local("not a url"));
        assert!(!endpoint_host_is_local("127.0.0.1:8888/search")); // no scheme
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
    fn warns_only_when_forced_unbrokered_and_local() {
        let local = Some("http://127.0.0.1:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, local).is_some());
        // Not force-routed: host egress reaches loopback fine.
        assert!(embed_local_warning(false, false, local).is_none());
        // Brokered: the embed-broker reaches the backend over its UDS.
        assert!(embed_local_warning(true, true, local).is_none());
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
    }
}
