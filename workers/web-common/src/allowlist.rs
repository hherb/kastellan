//! Host allowlist matching for net-egress workers.
//!
//! Entries come from the worker's allowlist env (a JSON array of strings —
//! `KASTELLAN_WEB_FETCH_ALLOWLIST`, `KASTELLAN_WEB_SEARCH_ALLOWLIST`, etc.),
//! injected by the host-side manifest from the `tool_allowlists` DB table.
//! Host forms:
//!   - `"en.wikipedia.org"` — exact host match only.
//!   - `".example.com"`     — the domain itself AND any subdomain.
//!
//! Matching is case-insensitive.
//!
//! Two parse entry points:
//!   - [`HostAllowlist::from_env_json`] — host-only matching (`is_allowed`),
//!     used by workers re-checking redirect hosts (web-fetch/web-search).
//!   - [`HostAllowlist::from_endpoints`] — `host[:port]` entries with
//!     port-scoped matching (`is_allowed_endpoint`), used by the egress proxy's
//!     CONNECT boundary so an allowlisted host is reachable only on its declared
//!     port (#241).

/// A parsed allowlist of host rules.
pub struct HostAllowlist {
    rules: Vec<Rule>,
}

/// One allowlist rule: a host matcher plus an optional port scope.
struct Rule {
    matcher: HostMatch,
    /// Explicit port from a `host:port` entry, or `None` for a bare-host entry
    /// (matches any port — the weaker, port-unconstrained grant).
    port: Option<u16>,
}

/// How a rule matches a host name.
enum HostMatch {
    /// Exact host, lowercased.
    Exact(String),
    /// Domain (without the leading dot), lowercased. Matches the domain itself
    /// and any subdomain.
    Suffix(String),
}

impl HostMatch {
    /// `host` must already be trimmed + lowercased.
    fn matches(&self, host: &str) -> bool {
        match self {
            HostMatch::Exact(x) => host == x,
            HostMatch::Suffix(d) => host == d || host.ends_with(&format!(".{d}")),
        }
    }
}

/// Parse a bare host token into a [`HostMatch`]. Returns `None` for empty or
/// lone-dot tokens (which are skipped).
fn parse_host(token: &str) -> Option<HostMatch> {
    let e = token.trim().to_lowercase();
    if e.is_empty() {
        return None;
    }
    if let Some(domain) = e.strip_prefix('.') {
        if domain.is_empty() {
            return None;
        }
        Some(HostMatch::Suffix(domain.to_string()))
    } else {
        Some(HostMatch::Exact(e))
    }
}

/// Split a `host[:port]` endpoint entry into its host token and optional port.
/// Handles bracketed IPv6 (`[::1]:443`, `[::1]`), `host:443` / `1.2.3.4:443`,
/// bare IPv6 (`::1` — no port), and bare hosts. A trailing `:<digits>` is a
/// port only when the host part has no unbracketed colon, so a bare IPv6
/// literal is never mis-split into host + bogus port.
fn split_host_port(entry: &str) -> (String, Option<u16>) {
    let e = entry.trim();
    // Bracketed IPv6: `[::1]` or `[::1]:443`.
    if let Some(rest) = e.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
            return (host.to_string(), port);
        }
    }
    match e.rsplit_once(':') {
        // Single trailing `:port` on a host with no other colon → host + port.
        Some((host, port_str)) if !host.contains(':') => match port_str.parse::<u16>() {
            Ok(port) => (host.to_string(), Some(port)),
            Err(_) => (e.to_string(), None),
        },
        // No colon, or multiple colons (bare IPv6) → host only.
        _ => (e.to_string(), None),
    }
}

impl HostAllowlist {
    /// Parse from the JSON-array env string, **host-only** (any `:port` is
    /// treated as part of the host token, not a scope). Used by workers that
    /// re-check redirect hosts; the port-scoped boundary check lives at the
    /// egress proxy via [`HostAllowlist::from_endpoints`]. Empty/blank entries
    /// are skipped.
    pub fn from_env_json(raw: &str) -> anyhow::Result<Self> {
        let entries: Vec<String> = serde_json::from_str(raw).map_err(|e| {
            anyhow::anyhow!("allowlist env is not a JSON array of strings: {e}")
        })?;
        let rules = entries
            .iter()
            .filter_map(|entry| parse_host(entry).map(|matcher| Rule { matcher, port: None }))
            .collect();
        Ok(Self { rules })
    }

    /// Parse `host[:port]` endpoint entries. A `:port` suffix scopes the grant
    /// to that exact port; a bare host matches any port (the weaker grant —
    /// flagged by [`HostAllowlist::is_port_scoped`]). Empty/blank entries are
    /// skipped.
    ///
    /// Parsing is infallible and **fail-closed**: an entry whose `:port` suffix
    /// is not a valid `u16` (e.g. a typo like `api.example.com:99999`) is *not*
    /// split — the whole string becomes the host token, yielding a dead
    /// `Exact("api.example.com:99999")` rule that no real host lookup can ever
    /// match. The grant is silently dropped rather than widened, so a malformed
    /// port can never over-permit; it can only fail to permit. (Callers that
    /// want to surface such typos to an operator should validate before calling
    /// — the egress-proxy allowlist is operator-authored, not attacker-supplied.)
    pub fn from_endpoints(entries: &[String]) -> Self {
        let rules = entries
            .iter()
            .filter_map(|entry| {
                let (host, port) = split_host_port(entry);
                parse_host(&host).map(|matcher| Rule { matcher, port })
            })
            .collect();
        Self { rules }
    }

    /// True iff `host` is permitted by any rule, **ignoring port**. Kept for
    /// back-compat and the literal-IP carve-out.
    pub fn is_allowed(&self, host: &str) -> bool {
        let h = host.trim().to_lowercase();
        self.rules.iter().any(|r| r.matcher.matches(&h))
    }

    /// True iff some rule permits `host` on `port`: the host matches AND the
    /// rule is either port-unconstrained (bare host) or names exactly `port`.
    pub fn is_allowed_endpoint(&self, host: &str, port: u16) -> bool {
        let h = host.trim().to_lowercase();
        self.rules.iter().any(|r| {
            r.matcher.matches(&h)
                && match r.port {
                    None => true,
                    Some(p) => p == port,
                }
        })
    }

    /// True iff every rule that matches `host` carries an explicit port — i.e.
    /// no bare host-only entry grants `host`. Lets the proxy surface a
    /// port-unconstrained grant in the audit trail instead of letting the
    /// weaker grant pass silently. `false` when no rule matches.
    pub fn is_port_scoped(&self, host: &str) -> bool {
        let h = host.trim().to_lowercase();
        let mut matched = false;
        for r in &self.rules {
            if r.matcher.matches(&h) {
                matched = true;
                if r.port.is_none() {
                    return false;
                }
            }
        }
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn al(entries: &[&str]) -> HostAllowlist {
        let json = serde_json::to_string(entries).unwrap();
        HostAllowlist::from_env_json(&json).unwrap()
    }

    fn eps(entries: &[&str]) -> HostAllowlist {
        let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        HostAllowlist::from_endpoints(&owned)
    }

    // ---- host-only matching (from_env_json / is_allowed) ------------------

    #[test]
    fn exact_matches_only_that_host() {
        let a = al(&["en.wikipedia.org"]);
        assert!(a.is_allowed("en.wikipedia.org"));
        assert!(!a.is_allowed("wikipedia.org"));
        assert!(!a.is_allowed("de.wikipedia.org"));
        assert!(!a.is_allowed("evil-en.wikipedia.org"));
    }

    #[test]
    fn leading_dot_matches_domain_and_subdomains() {
        let a = al(&[".example.com"]);
        assert!(a.is_allowed("example.com"));
        assert!(a.is_allowed("a.example.com"));
        assert!(a.is_allowed("a.b.example.com"));
    }

    #[test]
    fn leading_dot_does_not_match_lookalikes() {
        let a = al(&[".example.com"]);
        assert!(!a.is_allowed("evil-example.com"));
        assert!(!a.is_allowed("examplexcom"));
        assert!(!a.is_allowed("notexample.com"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let a = al(&["en.wikipedia.org", ".example.com"]);
        assert!(a.is_allowed("EN.Wikipedia.ORG"));
        assert!(a.is_allowed("A.Example.Com"));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let a = al(&[]);
        assert!(!a.is_allowed("example.com"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(HostAllowlist::from_env_json("not json").is_err());
    }

    #[test]
    fn whitespace_padded_entry_is_trimmed() {
        let a = al(&[" example.com "]);
        assert!(a.is_allowed("example.com"));
    }

    #[test]
    fn lone_dot_entry_is_ignored() {
        let a = al(&["."]);
        assert!(!a.is_allowed("example.com"));
        assert!(!a.is_allowed(""));
    }

    // ---- port-scoped matching (from_endpoints) — #241 ---------------------

    #[test]
    fn endpoint_match_requires_port() {
        let al = eps(&["api.example.com:443"]);
        assert!(al.is_allowed_endpoint("api.example.com", 443));
        assert!(
            !al.is_allowed_endpoint("api.example.com", 22),
            "same host, wrong port must be denied"
        );
    }

    #[test]
    fn endpoint_match_wildcard_host_any_declared_port() {
        let al = eps(&[".example.com:443"]);
        assert!(al.is_allowed_endpoint("docs.example.com", 443));
        assert!(!al.is_allowed_endpoint("docs.example.com", 80));
    }

    #[test]
    fn endpoint_without_port_in_entry_allows_any_port_back_compat() {
        // A bare host entry (no :port) keeps host-only semantics. This is the
        // port-UNCONSTRAINED grant — it exists only for the literal-IP carve-out
        // and legacy entries; force-routed worker allowlists are always
        // host:port (Stage 4), so this weaker form is not reachable for them.
        let al = eps(&["legacy.example.com"]);
        assert!(al.is_allowed_endpoint("legacy.example.com", 8443));
    }

    #[test]
    fn entry_kind_distinguishes_port_scoped_from_host_only() {
        // The matcher reports WHY it matched so the proxy can flag a
        // port-unconstrained grant in the audit trail (Task 3.2).
        let scoped = eps(&["a.com:443"]);
        let bare = eps(&["a.com"]);
        assert!(scoped.is_port_scoped("a.com"));
        assert!(!bare.is_port_scoped("a.com"));
    }

    #[test]
    fn endpoint_literal_ipv4_with_port() {
        let al = eps(&["127.0.0.1:8888"]);
        assert!(al.is_allowed_endpoint("127.0.0.1", 8888));
        assert!(!al.is_allowed_endpoint("127.0.0.1", 443));
        assert!(al.is_port_scoped("127.0.0.1"));
    }

    #[test]
    fn endpoint_bracketed_ipv6_with_port() {
        let al = eps(&["[::1]:443"]);
        assert!(al.is_allowed_endpoint("::1", 443));
        assert!(!al.is_allowed_endpoint("::1", 80));
    }

    #[test]
    fn endpoint_bare_ipv6_has_no_port_scope() {
        // A bare IPv6 literal (multiple colons, no brackets) must not be
        // mis-split into host + bogus port.
        let al = eps(&["::1"]);
        assert!(al.is_allowed_endpoint("::1", 443));
        assert!(al.is_allowed_endpoint("::1", 80));
        assert!(!al.is_port_scoped("::1"));
    }

    #[test]
    fn is_port_scoped_false_when_no_match() {
        let al = eps(&["a.com:443"]);
        assert!(!al.is_port_scoped("other.com"));
    }

    #[test]
    fn endpoint_out_of_range_port_is_a_dead_rule_fail_closed() {
        // A typo'd port (> u16::MAX) is NOT split; the whole string becomes an
        // exact host token that no real lookup matches. The grant is dropped,
        // never widened — fail-closed. (99999 can't even be expressed as a u16
        // at a call site, underscoring that no reachable port is granted.)
        let al = eps(&["api.example.com:99999"]);
        assert!(
            !al.is_allowed_endpoint("api.example.com", 443),
            "the malformed entry must NOT silently grant a real port"
        );
        assert!(
            !al.is_allowed("api.example.com"),
            "host-only check must not match the host either — it's a dead rule"
        );
    }
}
