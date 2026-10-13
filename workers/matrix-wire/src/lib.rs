//! Shared serde wire types for the Matrix channel: the only contract the
//! sandboxed `kastellan-worker-matrix` and the core-side `MatrixChannel` driver
//! share. Pure serde — no logic, no I/O — so both sides depend on it without
//! drift. The worker is a pure JSON-RPC server over these shapes
//! (`matrix.init` / `matrix.poll` / `matrix.send` / `matrix.drain`); see
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

/// `matrix.drain` result (#835): what the worker still held when the core
/// stopped polling it, asked once as the core's driver exits. The worker
/// stops its background sync first, so nothing more arrives after it answers
/// — unless the answer is `interrupted`, which also covers a stop that could
/// not be confirmed within [`DRAIN_STOP_BOUND_MS`].
///
/// **A count only** — never a room, a sender or a body: every one of those is
/// peer-supplied (the same rule as the `channel.inbound_dropped` row the core
/// writes from it). Deliberately no `Default`: its value would be the
/// all-clear answer, and an unknown count must never fall back to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainResult {
    /// Inbound events still queued in the worker, which no poll will now
    /// return. They are discarded with the worker.
    pub discarded: usize,
    /// The sync was stopped while it was handing one sync response's events
    /// to the queue (or the stop could not be confirmed). The homeserver
    /// will not send that response again, so an unknown number of its events
    /// — possibly none — are lost, and they are **not** in `discarded`.
    pub interrupted: bool,
}

/// How long the worker may take to answer `matrix.drain`: to stop its sync
/// and confirm the stop (#835). Past it, the answer is `interrupted`. Shared
/// with the core because its shutdown waits for the Matrix driver to finish
/// one long-poll **and** this drain (`DRAIN_BOUND`, asserted there).
pub const DRAIN_STOP_BOUND_MS: u64 = 800;

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

/// What [`push_bounded_fair`] did with the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FairPush {
    /// Buffered.
    Pushed,
    /// Buffered, evicting the oldest event overall (the buffer was at `cap`).
    PushedEvictedOldest,
    /// Refused: this sender already holds `per_peer_cap` events. The NEW event
    /// is dropped, nobody else's is.
    DroppedSenderFlood,
}

/// [`push_bounded`] with per-sender fairness (security audit 2026-09-02,
/// channel F1 / workers 1): drop-oldest alone let any one sender flood the
/// buffer and evict the operator's queued commands between two core polls. A
/// sender at `per_peer_cap` has its *own* newest event refused instead; the
/// global `cap` still applies afterwards.
pub fn push_bounded_fair(
    buf: &mut VecDeque<Event>,
    event: Event,
    cap: usize,
    per_peer_cap: usize,
) -> FairPush {
    if per_peer_cap > 0 && buf.iter().filter(|e| e.peer == event.peer).count() >= per_peer_cap {
        return FairPush::DroppedSenderFlood;
    }
    if push_bounded(buf, event, cap) {
        FairPush::PushedEvictedOldest
    } else {
        FairPush::Pushed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_from(peer: &str, body: &str) -> Event {
        Event { conversation: "!room:srv".into(), peer: peer.into(), body: body.into() }
    }

    /// Both sides share this type, but a worker binary built before a rename
    /// (version skew) and the fixtures that spell the fields by hand
    /// (`fake_matrix_worker.rs`, the driver's test fake) would not follow
    /// one: pinned so a rename is deliberate.
    #[test]
    fn drain_result_wire_shape_is_pinned() {
        let v = serde_json::to_value(DrainResult { discarded: 3, interrupted: true }).unwrap();
        assert_eq!(v, serde_json::json!({ "discarded": 3, "interrupted": true }));
    }

    #[test]
    fn fair_push_refuses_a_flooding_sender_and_keeps_others() {
        let mut buf = VecDeque::new();
        for i in 0..4 {
            assert_eq!(push_bounded_fair(&mut buf, ev_from("@evil:srv", &i.to_string()), 256, 4), FairPush::Pushed);
        }
        // The 5th from the same sender is refused; the buffer is unchanged.
        assert_eq!(push_bounded_fair(&mut buf, ev_from("@evil:srv", "5"), 256, 4), FairPush::DroppedSenderFlood);
        assert_eq!(buf.len(), 4);
        // The operator's event still gets in.
        assert_eq!(push_bounded_fair(&mut buf, ev_from("@op:srv", "hi"), 256, 4), FairPush::Pushed);
        assert_eq!(buf.len(), 5);
        assert!(buf.iter().any(|e| e.peer == "@op:srv"));
    }

    #[test]
    fn fair_push_still_honours_the_global_cap() {
        let mut buf = VecDeque::new();
        push_bounded_fair(&mut buf, ev_from("@a:srv", "1"), 2, 8);
        push_bounded_fair(&mut buf, ev_from("@b:srv", "2"), 2, 8);
        assert_eq!(push_bounded_fair(&mut buf, ev_from("@c:srv", "3"), 2, 8), FairPush::PushedEvictedOldest);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.front().unwrap().peer, "@b:srv");
    }

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
