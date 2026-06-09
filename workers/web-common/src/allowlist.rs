//! Host allowlist matching for net-egress workers.
//!
//! Entries come from the worker's allowlist env (a JSON array of strings —
//! `HHAGENT_WEB_FETCH_ALLOWLIST`, `HHAGENT_WEB_SEARCH_ALLOWLIST`, etc.),
//! injected by the host-side manifest from the `tool_allowlists` DB table.
//! Two forms:
//!   - `"en.wikipedia.org"` — exact host match only.
//!   - `".example.com"`     — the domain itself AND any subdomain.
//!
//! Matching is case-insensitive.

/// A parsed allowlist of host rules.
pub struct HostAllowlist {
    rules: Vec<Rule>,
}

enum Rule {
    /// Exact host, lowercased.
    Exact(String),
    /// Domain (without the leading dot), lowercased. Matches the domain itself
    /// and any subdomain.
    Suffix(String),
}

impl HostAllowlist {
    /// Parse from the JSON-array env string. Empty/blank entries are skipped.
    pub fn from_env_json(raw: &str) -> anyhow::Result<Self> {
        let entries: Vec<String> = serde_json::from_str(raw).map_err(|e| {
            anyhow::anyhow!("allowlist env is not a JSON array of strings: {e}")
        })?;
        let mut rules = Vec::new();
        for entry in entries {
            let e = entry.trim().to_lowercase();
            if e.is_empty() {
                continue;
            }
            if let Some(domain) = e.strip_prefix('.') {
                if !domain.is_empty() {
                    rules.push(Rule::Suffix(domain.to_string()));
                }
            } else {
                rules.push(Rule::Exact(e));
            }
        }
        Ok(Self { rules })
    }

    /// True iff `host` is permitted by any rule.
    pub fn is_allowed(&self, host: &str) -> bool {
        let h = host.trim().to_lowercase();
        self.rules.iter().any(|r| match r {
            Rule::Exact(x) => h == *x,
            Rule::Suffix(d) => h == *d || h.ends_with(&format!(".{d}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn al(entries: &[&str]) -> HostAllowlist {
        let json = serde_json::to_string(entries).unwrap();
        HostAllowlist::from_env_json(&json).unwrap()
    }

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
}
