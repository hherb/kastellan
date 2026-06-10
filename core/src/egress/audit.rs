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
