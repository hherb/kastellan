//! Resolve-time endpoint guard for force-routed workers (#452 / #429).
//!
//! When a `Net::Allowlist` worker's egress is force-routed through the host
//! egress proxy, the proxy's SSRF/DNS-rebinding defense range-checks every
//! **resolved hostname** — but an operator-allowlisted **literal IP** is
//! dialed with the range check deliberately skipped (the allowlisted-literal
//! carve-out in `egress-proxy::proxy::decide`; operator intent is explicit and
//! a literal cannot be rebound). So a literal endpoint — loopback included —
//! is dialable by the proxy when force-routed. (Worker-side rules still apply
//! on top: web-common's `validate_endpoint` allows plain `http` only for
//! loopback hosts and requires the host on the worker's allowlist — web-search
//! derives that allowlist from the endpoint itself, web-research reads its
//! operator `tool_allowlists` row, so its remedies must say to update the row
//! too.)
//!
//! The one endpoint class that is statically knowable to be dead is an RFC
//! 6761 `localhost` / `*.localhost` **name**: it takes the proxy's hostname
//! path, always resolves to loopback, and is range-denied on every CONNECT.
//! The worker then registers and looks healthy while every request fails — a
//! silent footgun (#452). The helpers here let a manifest refuse that dead
//! configuration at `resolve()` time (`Resolution::Misconfigured`), or warn
//! where the same class only degrades behaviour (#429, web-research's
//! optional embed endpoint — its message lives in `web_research.rs` beside
//! the env names it cites).
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
/// allowlisted-literal carve-out dials them, so the proxy serves them when
/// force-routed. A real remote hostname also returns `false` (resolve-time
/// cannot know its address without DNS; the connect-time proxy check owns that
/// case), and so does an unset, unparseable, or **host-less** endpoint — note
/// a scheme-less `localhost:8888/search` *parses* with `localhost` as the
/// scheme and no host, so it lands here too; all of these keep today's
/// fail-closed worker startup behaviour (the worker's own `validate_endpoint`
/// rejects them with a precise error) instead of a guard message about the
/// wrong problem.
pub(crate) fn endpoint_is_localhost_name(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else { return false };
    match url.host() {
        Some(Host::Domain(d)) => is_local_domain(d),
        // Ipv4/Ipv6 literals: dialable via the proxy's carve-out; no host
        // (including the scheme-less `localhost:8888` trap): the worker's own
        // endpoint validation owns that failure — no guard business either way.
        _ => false,
    }
}

/// Host-level flavour of [`endpoint_is_localhost_name`] for bare allowlist
/// entries (no URL around them). Tolerates the wildcard leading-dot form
/// (`.localhost`) since it ends with the suffix, and the FQDN trailing dot.
pub(crate) fn host_is_localhost_name(host: &str) -> bool {
    is_local_domain(host)
}

/// RFC 6761: `localhost` and any `*.localhost` name always resolve to loopback.
fn is_local_domain(domain: &str) -> bool {
    let d = domain.trim_end_matches('.'); // tolerate a FQDN trailing dot
    d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost")
}

/// True iff a `Net::Allowlist` worker spawned by this daemon will have its
/// egress force-routed through the host egress proxy: in host mode iff the
/// operator enabled `KASTELLAN_EGRESS_FORCE_ROUTING`; in micro-VM mode
/// treated as always-on. Strictly, `linux_firecracker/plan.rs` guarantees a
/// `Net::Allowlist` VM worker is never given a direct NIC — it fail-closed
/// REFUSES to boot without the egress proxy — so with the flag unset a VM
/// worker cannot spawn at all rather than being routed; either way no direct
/// route to a `localhost` name ever exists in VM mode, which is what the
/// guard needs to know. The flag name and truthiness parse are
/// `force_route`'s own (`ENV_ENABLE` / `env_flag_enabled`), so this mirror
/// cannot drift from the spawn path.
pub(crate) fn egress_will_force_route(
    is_microvm: bool,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> bool {
    is_microvm || force_route::env_flag_enabled(get_env(force_route::ENV_ENABLE))
}

/// The shared #452 `Misconfigured` message builder: `Some(detail)` iff the
/// worker's egress will be force-routed AND `endpoint` uses a `localhost`
/// name. The predicate composition and the explanation live here — once —
/// while each manifest passes its own env-var name and remedy sentence
/// (`remedy`), which is the only genuinely per-worker part. Callers that have
/// a broker escape hatch (web-search) apply that exemption at the call site.
pub(crate) fn forced_localhost_misconfig(
    endpoint_env: &str,
    endpoint: &str,
    force_routed: bool,
    remedy: &str,
) -> Option<String> {
    if !force_routed || !endpoint_is_localhost_name(endpoint) {
        return None;
    }
    Some(format!(
        "{endpoint_env} ({endpoint}) uses a `localhost` name, but this worker's \
         egress is force-routed through the egress proxy, which range-denies \
         what a localhost name resolves to (SSRF/DNS-rebinding defense) — the \
         tool would register but every request to it would fail. {remedy}"
    ))
}

/// Outcome of the generic #459 screen over a registered worker's
/// `Net::Allowlist` entries. Severity is data-driven — no per-worker hooks
/// (per-manifest copies are how the #452 gap happened in the first place):
/// the whole list dead ⇒ the tool is statically unreachable ⇒ refuse; a
/// proper subset dead ⇒ the tool still works for the live hosts ⇒ warn.
#[derive(Debug)]
pub(crate) enum NetScreen {
    /// Nothing to flag: not force-routed, empty list (the broker/zero-egress
    /// posture), or no `localhost`-name entries.
    Ok,
    /// A proper subset of entries is statically dead: register the tool but
    /// warn, naming the dead entries.
    Warn { dead: Vec<String> },
    /// Every entry is statically dead: the caller must treat this exactly
    /// like [`crate::worker_manifest::Resolution::Misconfigured`].
    Refuse { detail: String },
}

/// Host part of a `Net::Allowlist` entry. Entries today are `host:port`
/// (the entry builders' `format!` / web-fetch's `allowlist_to_net_entries`),
/// bare domains (browser-driver passes DB rows verbatim), or bracketed IPv6
/// forms. Strips one trailing `:<digits>` port; anything else is returned
/// whole — an unrecognized shape then classifies `false`, which is the safe
/// no-flag direction (the connect-time proxy check still owns it).
fn host_of_entry(entry: &str) -> &str {
    if entry.starts_with('[') {
        // Bracketed IPv6 literal, with or without a port suffix.
        if let Some(end) = entry.find(']') {
            return &entry[..=end];
        }
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            host
        }
        _ => entry,
    }
}

/// The generic #459 screen: classify every allowlist entry with
/// [`host_is_localhost_name`] (via [`host_of_entry`]) when `force_routed`.
/// See [`NetScreen`] for the severity policy. Like the rest of this module:
/// **no DNS** — only the statically-dead RFC 6761 name class is flagged, and
/// literal IPs are never flagged (the proxy's allowlisted-literal carve-out
/// dials them).
pub(crate) fn screen_net_allowlist(
    tool: &str,
    entries: &[String],
    force_routed: bool,
) -> NetScreen {
    if !force_routed || entries.is_empty() {
        return NetScreen::Ok;
    }
    let dead: Vec<String> = entries
        .iter()
        .filter(|e| host_is_localhost_name(host_of_entry(e)))
        .cloned()
        .collect();
    if dead.is_empty() {
        return NetScreen::Ok;
    }
    if dead.len() == entries.len() {
        let hosts = dead.join(", ");
        return NetScreen::Refuse {
            detail: format!(
                "every Net::Allowlist entry for {tool} uses a `localhost` name \
                 ({hosts}), but its egress is force-routed through the egress \
                 proxy, which range-denies what a localhost name resolves to \
                 (SSRF/DNS-rebinding defense) — the tool would register but \
                 every request would fail. Use literal-IP entries (the proxy \
                 dials an operator-allowlisted literal) or routable hostnames, \
                 and update the matching tool_allowlists rows / endpoint env \
                 vars to agree."
            ),
        };
    }
    NetScreen::Warn { dead }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_literals_are_not_flagged_even_when_loopback_or_private() {
        // The egress proxy's operator-allowlisted-literal carve-out
        // (`egress-proxy::proxy::decide`) dials an allowlisted literal IP with
        // the SSRF range check skipped — so at the PROXY layer a literal
        // endpoint is dialable when force-routed and must never be flagged as
        // statically dead. (Worker-side rules are separate and still apply:
        // plain `http` is loopback-only and the host must be allowlisted, so
        // e.g. the http+private rows below would be SchemeDenied by the worker
        // itself — not the proxy — which is out of this predicate's scope.)
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
    fn unset_unparseable_or_hostless_endpoints_are_not_flagged() {
        assert!(!endpoint_is_localhost_name(""));
        assert!(!endpoint_is_localhost_name("not a url")); // Url::parse Err
        // Url::parse Err too: a scheme cannot start with a digit.
        assert!(!endpoint_is_localhost_name("127.0.0.1:8888/search"));
        // The realistic scheme-less typo: this PARSES (scheme `localhost`,
        // host None) and takes the `_ => false` arm — the worker's own
        // validate_endpoint rejects it at startup ("endpoint has no host"),
        // which is the diagnostic the operator should get.
        assert!(!endpoint_is_localhost_name("localhost:8888/search"));
    }

    #[test]
    fn allowlist_hosts_classify_like_endpoint_hosts() {
        assert!(host_is_localhost_name("localhost"));
        assert!(host_is_localhost_name("LOCALHOST"));
        assert!(host_is_localhost_name("foo.localhost"));
        assert!(host_is_localhost_name(".localhost")); // wildcard entry form
        assert!(host_is_localhost_name("localhost.")); // FQDN trailing dot
        assert!(!host_is_localhost_name("docs.example.org"));
        assert!(!host_is_localhost_name(".example.org"));
        assert!(!host_is_localhost_name("127.0.0.1")); // literal: carve-out
        assert!(!host_is_localhost_name("localhost.attacker.example"));
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
    fn misconfig_builder_composes_env_endpoint_and_remedy() {
        let d = forced_localhost_misconfig(
            "KASTELLAN_TEST_ENDPOINT",
            "http://localhost:8888/search",
            true,
            "Do the remedy thing.",
        )
        .expect("forced + localhost name must produce a detail");
        assert!(d.contains("KASTELLAN_TEST_ENDPOINT"), "detail: {d}");
        assert!(d.contains("http://localhost:8888/search"), "detail: {d}");
        assert!(d.contains("Do the remedy thing."), "detail: {d}");
        // Not force-routed, or not a localhost name: no message.
        assert!(forced_localhost_misconfig(
            "E",
            "http://localhost:8888/search",
            false,
            "r"
        )
        .is_none());
        assert!(
            forced_localhost_misconfig("E", "http://127.0.0.1:8888/search", true, "r").is_none()
        );
    }

    #[test]
    fn host_of_entry_strips_only_a_trailing_digit_port() {
        assert_eq!(host_of_entry("localhost:443"), "localhost");
        assert_eq!(host_of_entry("localhost"), "localhost"); // bare DB row (browser-driver)
        assert_eq!(host_of_entry("example.org:8080"), "example.org");
        assert_eq!(host_of_entry("[::1]:443"), "[::1]"); // bracketed v6 stays whole
        assert_eq!(host_of_entry("[::1]"), "[::1]");
        // Defensive: a non-digit "port" is not a port — return the entry as-is
        // (it then classifies false, which is the safe no-flag direction).
        assert_eq!(host_of_entry("example.org:https"), "example.org:https");
    }

    #[test]
    fn screen_is_ok_when_not_forced_or_list_is_empty() {
        let dead_only = vec!["localhost:443".to_string()];
        assert!(matches!(screen_net_allowlist("t", &dead_only, false), NetScreen::Ok));
        // Empty allowlist = the broker/zero-egress posture: deliberately exempt.
        assert!(matches!(screen_net_allowlist("t", &[], true), NetScreen::Ok));
    }

    #[test]
    fn screen_is_ok_when_no_entry_is_a_localhost_name() {
        let entries = vec![
            "searx.example.org:8888".to_string(),
            "docs.example.org".to_string(), // bare row form
            "127.0.0.1:8888".to_string(),   // literal: carve-out, never flagged
            "[::1]:443".to_string(),        // v6 literal: carve-out
        ];
        assert!(matches!(screen_net_allowlist("t", &entries, true), NetScreen::Ok));
    }

    #[test]
    fn screen_warns_on_a_proper_subset_of_dead_entries() {
        let entries = vec![
            "docs.example.org:443".to_string(),
            "localhost:443".to_string(),
            "foo.localhost".to_string(),
        ];
        match screen_net_allowlist("t", &entries, true) {
            NetScreen::Warn { dead } => {
                assert_eq!(dead, vec!["localhost:443".to_string(), "foo.localhost".to_string()]);
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn screen_refuses_when_every_entry_is_dead() {
        let entries = vec!["localhost:443".to_string(), "svc.localhost:8080".to_string()];
        match screen_net_allowlist("deadtool", &entries, true) {
            NetScreen::Refuse { detail } => {
                assert!(detail.contains("deadtool"), "detail: {detail}");
                assert!(detail.contains("localhost:443"), "detail: {detail}");
                assert!(detail.contains("svc.localhost:8080"), "detail: {detail}");
                assert!(detail.contains("literal"), "remedy missing: {detail}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }
}
