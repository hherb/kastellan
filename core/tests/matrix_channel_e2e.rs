//! Hermetic full-loop e2e for the Matrix channel: spawns the `fake_matrix_worker`
//! example as a real child process, speaks the real `matrix.*` JSON-RPC over real
//! pipes through `ProtocolWorkerClient` + `MatrixChannel`, and drives it through
//! the real `ChannelBus` with the slice-#1 fake DB seams. Proves the spawn,
//! protocol, driver, bus, and routing integration with **no matrix-rust-sdk, no
//! homeserver, no sandbox, no Postgres**. Skip-as-pass if the fixture is unbuilt.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use kastellan_core::channel::auth::StaticPairings;
use kastellan_core::channel::bus::{ChannelBus, ChannelEvents, CompletedTasks};
use kastellan_core::channel::matrix::{MatrixChannel, ProtocolWorkerClient};
use kastellan_core::channel::{ChannelId, PeerId};
use kastellan_db::tasks::Lane;
use kastellan_protocol::client::Client;

/// Locate the `fake_matrix_worker` example binary (`<target>/debug/examples/…`).
fn fixture_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // core/
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("examples").join("fake_matrix_worker")
}

// ── slice-#1 fake DB seams (shared `enqueued` lets the completion echo routing). ──
#[derive(Clone, Default)]
struct FakeEvents {
    enqueued: Arc<Mutex<Vec<Value>>>,
}
#[async_trait::async_trait]
impl ChannelEvents for FakeEvents {
    async fn enqueue(&self, _lane: Lane, payload: Value) -> anyhow::Result<i64> {
        self.enqueued.lock().unwrap().push(payload);
        Ok(1)
    }
    async fn audit(&self, _action: &str, _payload: Value) {}
}

struct FakeCompleted {
    enqueued: Arc<Mutex<Vec<Value>>>,
    yielded: bool,
}
#[async_trait::async_trait]
impl CompletedTasks for FakeCompleted {
    async fn next_completed(&mut self) -> Option<i64> {
        if self.yielded {
            std::future::pending::<()>().await;
        }
        loop {
            if !self.enqueued.lock().unwrap().is_empty() {
                self.yielded = true;
                return Some(1);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn load(&self, _id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        let payload = self.enqueued.lock().unwrap().first().cloned();
        Ok(payload.map(|p| (p, Some(json!({"kind": "completed", "message": "pong"})))))
    }
}

/// Spawn the fixture worker (plain child, piped stdio) wired into a MatrixChannel.
fn spawn_matrix_channel(sent_file: &PathBuf, peer: &str) -> Option<MatrixChannel> {
    let bin = fixture_bin();
    if !bin.exists() {
        eprintln!(
            "\n[SKIP] fixture not built: {} — run `cargo build -p kastellan-worker-matrix --examples`\n",
            bin.display()
        );
        return None;
    }
    let child = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("FAKE_MATRIX_SENT", sent_file)
        .env("FAKE_MATRIX_PEER", peer)
        .env("FAKE_MATRIX_ROOM", "!room:srv")
        .env("FAKE_MATRIX_BODY", "hello from peer")
        .spawn()
        .expect("spawn fake matrix worker");
    let client = Client::from_child(child).expect("connect to fake worker");
    Some(MatrixChannel::new(ChannelId("matrix".into()), Box::new(ProtocolWorkerClient::new(client))))
}

#[tokio::test]
async fn paired_inbound_round_trips_to_a_sent_reply() {
    let sent_file = std::env::temp_dir().join(format!("kastellan-matrix-sent-{}", std::process::id()));
    let _ = std::fs::remove_file(&sent_file);

    let Some(ch) = spawn_matrix_channel(&sent_file, "@me:srv") else { return };

    let events = FakeEvents::default();
    let completed = FakeCompleted { enqueued: events.enqueued.clone(), yielded: false };
    let bus = ChannelBus::spawn(
        vec![Box::new(ch)],
        Arc::new(StaticPairings::from_peers([PeerId("@me:srv".into())])),
        None,
        Arc::new(events),
        Box::new(completed),
    );

    // The fixture's canned inbound is paired + benign → enqueued → completed →
    // routed reply "pong" → fixture appends it to sent_file. Poll until it lands.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while std::time::Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(&sent_file) {
            if !s.trim().is_empty() {
                got = Some(s);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bus.shutdown().await;
    let _ = std::fs::remove_file(&sent_file);

    let sent = got.expect("a reply should have been sent back to the worker");
    let line: Value = serde_json::from_str(sent.lines().next().unwrap()).unwrap();
    assert_eq!(line["conversation"], "!room:srv");
    assert_eq!(line["body"], "pong");
}

#[tokio::test]
async fn unpaired_inbound_is_dropped_no_reply() {
    let sent_file = std::env::temp_dir().join(format!("kastellan-matrix-unpaired-{}", std::process::id()));
    let _ = std::fs::remove_file(&sent_file);

    // Fixture emits from "@stranger:srv", which is NOT in the recognised set.
    let Some(ch) = spawn_matrix_channel(&sent_file, "@stranger:srv") else { return };

    let events = FakeEvents::default();
    let completed = FakeCompleted { enqueued: events.enqueued.clone(), yielded: false };
    let bus = ChannelBus::spawn(
        vec![Box::new(ch)],
        Arc::new(StaticPairings::from_peers([PeerId("@me:srv".into())])), // stranger not paired
        None,
        Arc::new(events.clone()),
        Box::new(completed),
    );

    // Give the loop time; the unpaired message must NOT enqueue and NOTHING is sent.
    tokio::time::sleep(Duration::from_millis(400)).await;
    bus.shutdown().await;

    assert!(events.enqueued.lock().unwrap().is_empty(), "unpaired peer must not enqueue a task");
    let sent = std::fs::read_to_string(&sent_file).unwrap_or_default();
    assert!(sent.trim().is_empty(), "no reply should be sent for an unpaired peer");
    let _ = std::fs::remove_file(&sent_file);
}
