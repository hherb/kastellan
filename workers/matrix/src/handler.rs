//! JSON-RPC handler for the matrix worker: `matrix.init` / `matrix.poll` /
//! `matrix.send`, dispatched over the [`MatrixSdk`] seam so the wire contract +
//! param validation are unit-tested without a homeserver.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde_json::Value;

use kastellan_matrix_wire::{PollParams, PollResult, SendParams};

use crate::sdk::MatrixSdk;

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
                self.sdk.send(&p.conversation, &p.body).map_err(|e| {
                    RpcError::new(codes::OPERATION_FAILED, format!("send failed: {e}"))
                })?;
                Ok(serde_json::json!({"ok": true}))
            }
            other => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use kastellan_matrix_wire::{Event, InitResult};

    /// Fake SDK: canned identity, a FIFO of queued inbound events, recorded sends.
    struct FakeSdk {
        queued: VecDeque<Event>,
        sent: Vec<(String, String)>,
    }
    impl FakeSdk {
        fn new(queued: Vec<Event>) -> Self {
            Self { queued: queued.into_iter().collect(), sent: vec![] }
        }
    }
    impl MatrixSdk for FakeSdk {
        fn identity(&self) -> InitResult {
            InitResult { user_id: "@bot:srv".into(), device_id: "DEV1".into() }
        }
        fn poll(&mut self, _timeout_ms: u64) -> Vec<Event> {
            self.queued.drain(..).collect()
        }
        fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()> {
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
