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
    /// Whether the proxy MITM-terminated this connection's TLS (slice #3a).
    /// Absent on slice #1/#2 lines → defaults false, so old streams still parse.
    #[serde(default)]
    tls_intercepted: bool,
}

/// An audit row ready for `kastellan_db::audit::insert` (actor + action + payload).
#[derive(Debug, PartialEq, Eq)]
pub struct EgressAuditRow {
    pub actor: &'static str,
    pub action: String,
    pub payload: serde_json::Value,
}

/// Drive an egress-proxy decision stream: read it line by line, map each valid
/// decision line to an [`EgressAuditRow`] via [`decision_to_audit`], and hand
/// the row to `on_row`. Lines that aren't valid decision JSON are skipped (a
/// compromised proxy emitting garbage can never widen anything — it only fails
/// to produce a row). Returns when the reader hits EOF or a read error.
///
/// Pure over its I/O source + sink, so the loop is unit-testable without
/// Postgres or a live proxy; the live wrapper ([`super::net_worker`]) supplies
/// an `on_row` that inserts into `audit_log`.
pub fn ingest_decisions_into<R, F>(reader: R, mut on_row: F)
where
    R: std::io::BufRead,
    F: FnMut(EgressAuditRow),
{
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Some(row) = decision_to_audit(&line) {
            on_row(row);
        }
    }
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
            "tls_intercepted": d.tls_intercepted,
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
    fn allowed_row_carries_tls_intercepted_flag() {
        let line = r#"{"worker":"web-fetch","host":"a.com","port":443,"resolved_ip":"1.2.3.4","verdict":"allowed","reason":"ok","tls_intercepted":true}"#;
        let row = decision_to_audit(line).unwrap();
        assert_eq!(row.payload["tls_intercepted"], true);
    }

    #[test]
    fn missing_tls_intercepted_defaults_false() {
        // Slice #1/#2 lines (no field) must still parse, defaulting to false.
        let line = r#"{"worker":"w","host":"h","port":443,"resolved_ip":null,"verdict":"allowed","reason":"ok"}"#;
        let row = decision_to_audit(line).unwrap();
        assert_eq!(row.payload["tls_intercepted"], false);
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

    #[test]
    fn ingest_maps_each_line_to_an_audit_row() {
        let input: &[u8] = b"{\"worker\":\"web-fetch\",\"host\":\"a.com\",\"port\":443,\"resolved_ip\":\"1.2.3.4\",\"verdict\":\"allowed\",\"reason\":\"ok\"}\n\
                             {\"worker\":\"web-fetch\",\"host\":\"b.com\",\"port\":443,\"resolved_ip\":null,\"verdict\":\"blocked_allowlist\",\"reason\":\"x\"}\n";
        let mut rows = Vec::new();
        ingest_decisions_into(input, |row| rows.push(row));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "egress.allowed");
        assert_eq!(rows[1].action, "egress.blocked.allowlist");
    }

    #[test]
    fn ingest_skips_garbage_and_unknown_verdict_lines() {
        // A compromised proxy emitting noise produces no rows for the noise —
        // it can never widen anything, only fail to record.
        let input: &[u8] = b"not json\n\
                             {\"verdict\":\"wat\"}\n\
                             {\"worker\":\"w\",\"host\":\"h\",\"port\":443,\"resolved_ip\":null,\"verdict\":\"blocked_ssrf\",\"reason\":\"r\"}\n";
        let mut rows = Vec::new();
        ingest_decisions_into(input, |row| rows.push(row));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "egress.blocked.ssrf");
    }
}
