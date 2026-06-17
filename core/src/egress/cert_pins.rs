//! Operator cert-pin config: parse `KASTELLAN_EGRESS_CERT_PINS` (the same
//! `{host:["sha256/<b64>"]}` JSON the egress-proxy sidecar enforces) into a
//! host-keyed map, and select the per-worker subset to hand each sidecar.
//!
//! Layering: this host-side parse is **structural only** — it checks the JSON
//! shape and the `sha256/` prefix so a malformed config fails the daemon closed
//! at startup, and so pins can be selected per worker. The authoritative strict
//! validation (base64 decode, 32-byte SPKI length) lives in the egress-proxy's
//! `PinSet::parse`; a pin with a good prefix but bad base64 passes here and
//! fails closed one layer later, at sidecar startup. Keeping one strict
//! validator (the proxy's) avoids drift.

use std::collections::BTreeMap;
use std::collections::HashSet;

/// Prefix every pin string must carry (RFC-7469 `sha256/<base64-SPKI>`).
const PIN_PREFIX: &str = "sha256/";

/// A parsed, structurally-valid operator pin config: lowercased host → its
/// non-empty list of `sha256/<b64>` pin strings.
///
/// Invariant: every value vec is non-empty (empty arrays are rejected by
/// [`parse_cert_pins`]). An all-empty *map* is possible only from `{}`; callers
/// normalize that to "no pins".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertPinMap(BTreeMap<String, Vec<String>>);

impl CertPinMap {
    /// True when no hosts are pinned (`{}`).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Structural failure parsing `KASTELLAN_EGRESS_CERT_PINS`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CertPinError {
    /// Not valid JSON, or not a JSON object of host -> array-of-strings.
    #[error("cert-pin config must be a JSON object of host -> [\"sha256/...\"]: {0}")]
    Shape(String),
    /// A host mapped to an empty pin array — unsatisfiable; almost always a
    /// misconfiguration (matches the proxy's own rejection).
    #[error("host {0:?} has an empty pin list")]
    EmptyPinList(String),
    /// A pin string did not start with the required `sha256/` prefix.
    #[error("host {host:?} pin {pin:?} is missing the `sha256/` prefix")]
    BadPrefix { host: String, pin: String },
}

/// Parse + structurally validate the operator pin JSON. See the module doc for
/// the layering (structural here; strict validation in the proxy).
pub fn parse_cert_pins(json: &str) -> Result<CertPinMap, CertPinError> {
    // serde rejects any non-object / non-array-of-strings shape for us.
    let raw: BTreeMap<String, Vec<String>> =
        serde_json::from_str(json).map_err(|e| CertPinError::Shape(e.to_string()))?;
    let mut out = BTreeMap::new();
    for (host, pins) in raw {
        if pins.is_empty() {
            return Err(CertPinError::EmptyPinList(host));
        }
        for pin in &pins {
            if !pin.starts_with(PIN_PREFIX) {
                return Err(CertPinError::BadPrefix { host: host.clone(), pin: pin.clone() });
            }
        }
        // DNS is case-insensitive; the proxy matches lowercased hosts.
        out.insert(host.to_ascii_lowercase(), pins);
    }
    Ok(CertPinMap(out))
}

/// Extract the host from an allowlist endpoint (`host:port`).
///
/// Allowlist entries are `host:port` (the shape the proxy + web-common use);
/// pins are keyed by bare host, so selection matches on the host alone. IPv6
/// literals must be bracketed (`[2001:db8::1]:443`) — the same convention the
/// allowlist uses. A bare host with no port is returned unchanged.
pub fn host_of_endpoint(endpoint: &str) -> &str {
    if let Some(rest) = endpoint.strip_prefix('[') {
        // `[ipv6]:port` or `[ipv6]` — host is between the brackets.
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
        return endpoint; // malformed bracket; hand back as-is
    }
    match endpoint.rsplit_once(':') {
        Some((host, _port)) => host,
        None => endpoint,
    }
}

/// Select the subset of `map` whose hosts appear in this worker's `allowlist`,
/// serialized back to the proxy's `{host:[...]}` JSON. Returns `None` when no
/// pinned host is in the allowlist, so the sidecar gets no pin env and the
/// no-pin path stays byte-identical.
///
/// Least-privilege: a worker's sidecar only learns pins for hosts that worker
/// may actually dial.
pub fn select_pins_for_allowlist(map: &CertPinMap, allowlist: &[String]) -> Option<String> {
    let hosts: HashSet<String> = allowlist
        .iter()
        .map(|ep| host_of_endpoint(ep).to_ascii_lowercase())
        .collect();
    let selected: BTreeMap<&String, &Vec<String>> =
        map.0.iter().filter(|(host, _)| hosts.contains(*host)).collect();
    if selected.is_empty() {
        return None;
    }
    // BTreeMap<&String,&Vec<String>> serializes as the same {host:[...]} object
    // the proxy parses. Serialization of an owned, in-memory map cannot fail.
    Some(serde_json::to_string(&selected).expect("pin submap serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_map_and_lowercases_hosts() {
        let m = parse_cert_pins(r#"{"API.Example.com":["sha256/AAAA"]}"#).unwrap();
        assert!(!m.is_empty());
        // Host key is stored lowercased.
        assert_eq!(m.0.get("api.example.com"), Some(&vec!["sha256/AAAA".to_string()]));
        assert_eq!(m.0.get("API.Example.com"), None);
    }

    #[test]
    fn empty_object_is_empty_map() {
        let m = parse_cert_pins("{}").unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn rejects_empty_pin_array() {
        let err = parse_cert_pins(r#"{"h.com":[]}"#).unwrap_err();
        assert_eq!(err, CertPinError::EmptyPinList("h.com".to_string()));
    }

    #[test]
    fn rejects_missing_sha256_prefix() {
        let err = parse_cert_pins(r#"{"h.com":["nope"]}"#).unwrap_err();
        assert_eq!(
            err,
            CertPinError::BadPrefix { host: "h.com".to_string(), pin: "nope".to_string() }
        );
    }

    #[test]
    fn rejects_non_object_shape() {
        assert!(matches!(parse_cert_pins("[]").unwrap_err(), CertPinError::Shape(_)));
        assert!(matches!(parse_cert_pins("\"x\"").unwrap_err(), CertPinError::Shape(_)));
        assert!(matches!(parse_cert_pins("{\"h\":5}").unwrap_err(), CertPinError::Shape(_)));
    }

    #[test]
    fn accepts_multiple_pins_per_host() {
        let m = parse_cert_pins(r#"{"h.com":["sha256/A","sha256/B"]}"#).unwrap();
        assert!(!m.is_empty());
        // Both pins must survive the parse → select round-trip.
        let json = select_pins_for_allowlist(&m, &["h.com:443".to_string()])
            .expect("pinned host in allowlist");
        assert!(json.contains("sha256/A"), "first pin dropped: {json}");
        assert!(json.contains("sha256/B"), "second pin dropped: {json}");
    }

    #[test]
    fn host_of_endpoint_strips_port() {
        assert_eq!(host_of_endpoint("api.example.com:443"), "api.example.com");
    }

    #[test]
    fn host_of_endpoint_handles_bracketed_ipv6() {
        assert_eq!(host_of_endpoint("[2001:db8::1]:8443"), "2001:db8::1");
        assert_eq!(host_of_endpoint("[::1]:443"), "::1");
    }

    #[test]
    fn host_of_endpoint_bracketed_ipv6_without_port() {
        assert_eq!(host_of_endpoint("[2001:db8::1]"), "2001:db8::1");
    }

    #[test]
    fn host_of_endpoint_bare_host_unchanged() {
        assert_eq!(host_of_endpoint("example.com"), "example.com");
    }

    #[test]
    fn select_keeps_only_allowlisted_hosts() {
        let map = parse_cert_pins(
            r#"{"a.com":["sha256/A"],"b.com":["sha256/B"]}"#,
        )
        .unwrap();
        let json = select_pins_for_allowlist(&map, &["a.com:443".to_string()]).unwrap();
        // Round-trips to exactly the a.com subset.
        let expected = parse_cert_pins(r#"{"a.com":["sha256/A"]}"#).unwrap();
        assert_eq!(parse_cert_pins(&json).unwrap(), expected);
    }

    #[test]
    fn select_is_case_insensitive_on_host() {
        let map = parse_cert_pins(r#"{"a.com":["sha256/A"]}"#).unwrap();
        let json = select_pins_for_allowlist(&map, &["A.COM:443".to_string()]).unwrap();
        assert_eq!(parse_cert_pins(&json).unwrap(), map);
    }

    #[test]
    fn select_no_intersection_is_none() {
        let map = parse_cert_pins(r#"{"a.com":["sha256/A"]}"#).unwrap();
        assert!(select_pins_for_allowlist(&map, &["z.com:443".to_string()]).is_none());
        assert!(select_pins_for_allowlist(&map, &[]).is_none());
    }
}
