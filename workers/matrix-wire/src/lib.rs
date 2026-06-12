//! Shared serde wire types for the Matrix channel: the only contract the
//! sandboxed `kastellan-worker-matrix` and the core-side `MatrixChannel` driver
//! share. Pure serde — no logic, no I/O — so both sides depend on it without
//! drift. The worker is a pure JSON-RPC server over these shapes
//! (`matrix.init` / `matrix.poll` / `matrix.send`); see
//! `docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md`.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// One decrypted inbound text message the worker surfaces to the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Matrix room id the message arrived in.
    pub conversation: String,
    /// Sender's matrix id (`@user:server`).
    pub peer: String,
    /// Decrypted plaintext body.
    pub body: String,
}

/// `matrix.poll` result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollResult {
    pub events: Vec<Event>,
}

/// `matrix.poll` params.
#[derive(Clone, Debug, Deserialize)]
pub struct PollParams {
    /// If the worker's buffer is empty, wait up to this long for the first event.
    #[serde(default = "default_poll_ms")]
    pub timeout_ms: u64,
}

fn default_poll_ms() -> u64 {
    2000
}

/// `matrix.send` params.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendParams {
    /// Target room id.
    pub conversation: String,
    /// Plaintext body to send (the worker E2E-encrypts it).
    pub body: String,
}

/// `matrix.init` result — confirms login + reports identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResult {
    pub user_id: String,
    pub device_id: String,
}

/// Push `event` onto a bounded inbound buffer, dropping the oldest entry when
/// `cap` is exceeded. Returns `true` iff an event was dropped (the worker logs a
/// counter so an operator can see when the channel is shedding load). A single-
/// user channel realistically never hits the cap; this is a backstop against a
/// flooding peer, mirroring the handoff cache's drop-with-warn backstop.
pub fn push_bounded(buf: &mut VecDeque<Event>, event: Event, cap: usize) -> bool {
    let dropped = buf.len() >= cap && cap > 0;
    if dropped {
        buf.pop_front();
    }
    buf.push_back(event);
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(body: &str) -> Event {
        Event {
            conversation: "!room:srv".into(),
            peer: "@me:srv".into(),
            body: body.into(),
        }
    }

    #[test]
    fn event_round_trips() {
        let e = ev("hello");
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }

    #[test]
    fn poll_result_round_trips() {
        let p = PollResult { events: vec![ev("a"), ev("b")] };
        let s = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<PollResult>(&s).unwrap(), p);
    }

    #[test]
    fn poll_params_defaults_timeout() {
        let p: PollParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.timeout_ms, 2000);
        let p: PollParams = serde_json::from_str(r#"{"timeout_ms": 500}"#).unwrap();
        assert_eq!(p.timeout_ms, 500);
    }

    #[test]
    fn send_params_requires_fields() {
        assert!(serde_json::from_str::<SendParams>(r#"{"conversation":"!r:s"}"#).is_err());
        let p: SendParams =
            serde_json::from_str(r#"{"conversation":"!r:s","body":"hi"}"#).unwrap();
        assert_eq!(p.conversation, "!r:s");
        assert_eq!(p.body, "hi");
    }

    #[test]
    fn init_result_round_trips() {
        let i = InitResult { user_id: "@bot:srv".into(), device_id: "DEV".into() };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<InitResult>(&s).unwrap(), i);
    }

    #[test]
    fn push_bounded_drops_oldest_past_cap() {
        let mut buf = VecDeque::new();
        assert!(!push_bounded(&mut buf, ev("1"), 2));
        assert!(!push_bounded(&mut buf, ev("2"), 2));
        assert!(push_bounded(&mut buf, ev("3"), 2)); // dropped "1"
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.front().unwrap().body, "2");
        assert_eq!(buf.back().unwrap().body, "3");
    }
}
