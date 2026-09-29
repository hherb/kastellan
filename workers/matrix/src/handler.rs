//! JSON-RPC handler for the matrix worker: `matrix.init` / `matrix.poll` /
//! `matrix.send`, dispatched over the [`MatrixSdk`] seam so the wire contract +
//! param validation are unit-tested without a homeserver.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde_json::Value;

use kastellan_matrix_wire::{PollParams, PollResult, SendParams};

use crate::sdk::{MatrixSdk, SendError, SendFailure};

/// The worker handler, generic over the SDK seam so tests inject a fake.
pub struct MatrixHandler<S: MatrixSdk> {
    sdk: S,
}

impl<S: MatrixSdk> MatrixHandler<S> {
    pub fn new(sdk: S) -> Self {
        Self { sdk }
    }
}

impl<S: MatrixSdk> Handler for MatrixHandler<S> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "matrix.init" => Ok(serde_json::to_value(self.sdk.identity())
                .expect("InitResult serialises")),
            "matrix.poll" => {
                let p: PollParams = serde_json::from_value(params).map_err(|e| {
                    RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}"))
                })?;
                let events = self.sdk.poll(p.timeout_ms);
                Ok(serde_json::to_value(PollResult { events }).expect("PollResult serialises"))
            }
            "matrix.send" => {
                let p: SendParams = serde_json::from_value(params).map_err(|e| {
                    RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}"))
                })?;
                self.sdk.send(&p.conversation, &p.body).map_err(send_error)?;
                Ok(serde_json::json!({"ok": true}))
            }
            other => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
    }
}

/// The wire answer for a failed send. The code tells the core whose problem
/// it is (#782): a room's refusal is `OPERATION_FAILED`, charged to that room;
/// a refused token and an unavailable homeserver are channel-wide
/// (`UPSTREAM_UNAVAILABLE`), and never count toward giving up a room's
/// replies.
///
/// ⚠️ **A refused token is `UPSTREAM_UNAVAILABLE`, not `UPSTREAM_AUTH_FAILED`,
/// on purpose.** The core answers `UPSTREAM_AUTH_FAILED` by starting a fresh
/// worker *while this one still runs* (so a renewed credential is read), which
/// for this worker means two matrix-sdk clients on one state + crypto store —
/// olm sessions can diverge, and E2E messages stop decrypting. A dead session
/// is recovered the way it always was: the sync loop fails on the same 401,
/// exits, and the supervisor's respawn (and its loud reports) take over.
fn send_error(e: SendError) -> RpcError {
    match e.failure {
        SendFailure::Conversation => {
            RpcError::new(codes::OPERATION_FAILED, format!("send failed: {}", e.detail))
        }
        SendFailure::Unavailable => RpcError::new(
            codes::UPSTREAM_UNAVAILABLE,
            format!("send failed: the homeserver is unavailable, for every room: {}", e.detail),
        ),
        SendFailure::Credential => RpcError::new(
            codes::UPSTREAM_UNAVAILABLE,
            format!(
                "send failed: the homeserver refused the bot's access token (HTTP 401), for every \
                 room: the Matrix session is invalid or revoked. Not a kastellan policy refusal; \
                 the operator must log the bot in again (kastellan-cli matrix probe). {}",
                e.detail
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::SendEvidence;
    use std::collections::VecDeque;

    use kastellan_matrix_wire::{Event, InitResult};

    /// Fake SDK: canned identity, a FIFO of queued inbound events, recorded
    /// sends. `fail`, when set, is what every send fails with.
    struct FakeSdk {
        queued: VecDeque<Event>,
        sent: Vec<(String, String)>,
        fail: Option<SendEvidence>,
    }
    impl FakeSdk {
        fn new(queued: Vec<Event>) -> Self {
            Self { queued: queued.into_iter().collect(), sent: vec![], fail: None }
        }
    }
    impl MatrixSdk for FakeSdk {
        fn identity(&self) -> InitResult {
            InitResult { user_id: "@bot:srv".into(), device_id: "DEV1".into() }
        }
        fn poll(&mut self, _timeout_ms: u64) -> Vec<Event> {
            self.queued.drain(..).collect()
        }
        fn send(&mut self, conversation: &str, body: &str) -> Result<(), SendError> {
            if let Some(evidence) = self.fail {
                return Err(SendError::new(evidence, "fake: send refused"));
            }
            self.sent.push((conversation.to_string(), body.to_string()));
            Ok(())
        }
    }

    fn ev(body: &str) -> Event {
        Event { conversation: "!room:srv".into(), peer: "@me:srv".into(), body: body.into() }
    }

    #[test]
    fn init_reports_identity() {
        let mut h = MatrixHandler::new(FakeSdk::new(vec![]));
        let out = h.call("matrix.init", serde_json::json!({})).unwrap();
        assert_eq!(out["user_id"], "@bot:srv");
        assert_eq!(out["device_id"], "DEV1");
    }

    #[test]
    fn poll_drains_queued_events() {
        let mut h = MatrixHandler::new(FakeSdk::new(vec![ev("a"), ev("b")]));
        let out = h.call("matrix.poll", serde_json::json!({"timeout_ms": 0})).unwrap();
        let events = out["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["body"], "a");
        // Second poll: buffer now empty.
        let out2 = h.call("matrix.poll", serde_json::json!({})).unwrap();
        assert!(out2["events"].as_array().unwrap().is_empty());
    }

    #[test]
    fn send_records_and_acks() {
        let mut h = MatrixHandler::new(FakeSdk::new(vec![]));
        let out = h
            .call("matrix.send", serde_json::json!({"conversation": "!r:s", "body": "hi"}))
            .unwrap();
        assert_eq!(out["ok"], true);
    }

    /// #782: the code on the wire is what decides whether the core charges a
    /// room or holds the whole channel, so each class must reach it.
    #[test]
    fn a_failed_send_answers_with_the_code_of_whoever_caused_it() {
        for (evidence, code) in [
            (SendEvidence::Status(403), codes::OPERATION_FAILED),
            (SendEvidence::Local, codes::OPERATION_FAILED),
            (SendEvidence::Status(503), codes::UPSTREAM_UNAVAILABLE),
            (SendEvidence::NoResponse, codes::UPSTREAM_UNAVAILABLE),
            // Not UPSTREAM_AUTH_FAILED: see `send_error`.
            (SendEvidence::Status(401), codes::UPSTREAM_UNAVAILABLE),
        ] {
            let mut sdk = FakeSdk::new(vec![]);
            sdk.fail = Some(evidence);
            let mut h = MatrixHandler::new(sdk);
            let err = h
                .call("matrix.send", serde_json::json!({"conversation": "!r:s", "body": "hi"}))
                .unwrap_err();
            assert_eq!(err.code, code, "{evidence:?}: {}", err.message);
        }
    }

    /// The 401 shares `UPSTREAM_UNAVAILABLE` with a dead homeserver, so its
    /// message is the only thing telling the operator to act.
    #[test]
    fn a_refused_token_says_the_operator_must_log_in_again() {
        let mut sdk = FakeSdk::new(vec![]);
        sdk.fail = Some(SendEvidence::Status(401));
        let err = MatrixHandler::new(sdk)
            .call("matrix.send", serde_json::json!({"conversation": "!r:s", "body": "hi"}))
            .unwrap_err();
        assert!(err.message.contains("HTTP 401"), "{}", err.message);
        assert!(err.message.contains("log the bot in again"), "{}", err.message);
    }

    #[test]
    fn send_missing_field_is_invalid_params() {
        let mut h = MatrixHandler::new(FakeSdk::new(vec![]));
        let err = h
            .call("matrix.send", serde_json::json!({"conversation": "!r:s"}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = MatrixHandler::new(FakeSdk::new(vec![]));
        let err = h.call("matrix.nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }
}
