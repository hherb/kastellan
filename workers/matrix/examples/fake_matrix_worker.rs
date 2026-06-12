//! Test fixture (NOT a production binary): a fake matrix worker that speaks the
//! real `matrix.init` / `matrix.poll` / `matrix.send` JSON-RPC surface over stdio
//! so `core/tests/matrix_channel_e2e.rs` can exercise the full
//! MatrixChannel → ChannelBus loop against a real worker process — with no
//! matrix-rust-sdk, no homeserver, no network, no sandbox.
//!
//! Behaviour (env-configured):
//! - emits exactly one canned inbound event on the first `matrix.poll`
//!   (peer = `FAKE_MATRIX_PEER`, room = `FAKE_MATRIX_ROOM`, body = `FAKE_MATRIX_BODY`),
//!   empty on every subsequent poll;
//! - appends each `matrix.send` as a JSON line to `FAKE_MATRIX_SENT` so the test
//!   can assert the routed reply was delivered.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use kastellan_protocol::{codes, server::serve_stdio, server::Handler, RpcError};
use serde_json::Value;

struct FakeWorker {
    emitted: AtomicBool,
    peer: String,
    room: String,
    body: String,
    sent_file: Option<String>,
}

impl Handler for FakeWorker {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "matrix.init" => Ok(serde_json::json!({"user_id": "@bot:srv", "device_id": "FAKE"})),
            "matrix.poll" => {
                if self.emitted.swap(true, Ordering::SeqCst) {
                    Ok(serde_json::json!({ "events": [] }))
                } else {
                    Ok(serde_json::json!({ "events": [{
                        "conversation": self.room,
                        "peer": self.peer,
                        "body": self.body,
                    }]}))
                }
            }
            "matrix.send" => {
                let conversation = params.get("conversation").and_then(Value::as_str).unwrap_or("");
                let body = params.get("body").and_then(Value::as_str).unwrap_or("");
                if let Some(path) = &self.sent_file {
                    if let Ok(mut f) =
                        std::fs::OpenOptions::new().create(true).append(true).open(path)
                    {
                        let line = serde_json::json!({"conversation": conversation, "body": body});
                        let _ = writeln!(f, "{line}");
                    }
                }
                Ok(serde_json::json!({"ok": true}))
            }
            other => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
    }
}

fn main() -> std::io::Result<()> {
    let mut h = FakeWorker {
        emitted: AtomicBool::new(false),
        peer: std::env::var("FAKE_MATRIX_PEER").unwrap_or_else(|_| "@me:srv".into()),
        room: std::env::var("FAKE_MATRIX_ROOM").unwrap_or_else(|_| "!room:srv".into()),
        body: std::env::var("FAKE_MATRIX_BODY").unwrap_or_else(|_| "hello from peer".into()),
        sent_file: std::env::var("FAKE_MATRIX_SENT").ok(),
    };
    serve_stdio(&mut h)
}
