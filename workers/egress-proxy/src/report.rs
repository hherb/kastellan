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
    /// True when this allowed CONNECT was TLS-terminated + re-originated by the
    /// proxy (MITM). False for blocks and for plaintext pass-through tunnels.
    /// Default false so existing constructors and the host-side audit stay
    /// backward-compatible. (Slice #3a — the only new plaintext-adjacent signal.)
    #[serde(default)]
    pub tls_intercepted: bool,
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
            tls_intercepted: false,
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
            tls_intercepted: false,
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
                tls_intercepted: false,
            });
        }
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.lines().count(), 1);
    }

    #[test]
    fn tls_intercepted_serializes_and_defaults_false() {
        let mut d = Decision {
            worker: "web-fetch".into(), host: "h".into(), port: 443,
            resolved_ip: None, verdict: Verdict::Allowed, reason: "ok".into(),
            tls_intercepted: true,
        };
        assert!(d.to_line().contains("\"tls_intercepted\":true"));
        d.tls_intercepted = false;
        let v: serde_json::Value = serde_json::from_str(&d.to_line()).unwrap();
        assert_eq!(v["tls_intercepted"], false);
    }
}
