//! Hermetic full-loop e2e for the Matrix channel: spawns the `fake_matrix_worker`
//! example as a real child process, speaks the real `matrix.*` JSON-RPC over real
//! pipes through the PRODUCTION stack — `PersistentWorker` (supervision) +
//! `PolledWorkerDriver` (poll/identity/pending) + `MatrixChannel` — and drives it
//! through the real `ChannelBus` with the slice-#1 fake DB seams. Proves the
//! spawn, protocol, driver, bus, and routing integration with **no
//! matrix-rust-sdk, no homeserver, no sandbox, no Postgres**. Skip-as-pass if
//! the fixture is unbuilt.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use kastellan_core::channel::auth::StaticPairings;
use kastellan_core::channel::bus::{
    ChannelBus, ChannelEvents, ClaimedReply, CompletedTasks, ReplyDisposition,
};
use kastellan_core::channel::matrix::{
    encode_matrix_send, parse_matrix_poll, MatrixChannel, MATRIX_POLLED_SPEC,
};
use kastellan_core::channel::polled_driver::{
    DriverAudit, InboundDropStage, InboundDropped, PolledWorkerDriver,
};
use kastellan_core::channel::{ChannelId, PeerId};
use kastellan_core::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
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
    async fn audit(&self, _action: &'static str, _payload: Value) {}
}

struct FakeCompleted {
    enqueued: Arc<Mutex<Vec<Value>>>,
    yielded: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl CompletedTasks for FakeCompleted {
    async fn next_completed(&self) -> Option<i64> {
        if self.yielded.load(std::sync::atomic::Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        loop {
            if !self.enqueued.lock().unwrap().is_empty() {
                self.yielded.store(true, std::sync::atomic::Ordering::SeqCst);
                return Some(1);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn load(&self, _id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        let payload = self.enqueued.lock().unwrap().first().cloned();
        Ok(payload.map(|p| (p, Some(json!({"kind": "completed", "message": "pong"})))))
    }
    /// Claims always succeed: this fake predates the reply claim (#825),
    /// whose own semantics are tested against `bus/tests/catch_up.rs`'s
    /// `Backlog` and real Postgres.
    async fn claim(&self, _id: i64, _d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        let now = time::OffsetDateTime::now_utc();
        Ok(Some(ClaimedReply { created_at: now, finished_at: Some(now) }))
    }
    async fn unsettled(&self, _after_id: i64, _limit: i64) -> anyhow::Result<Vec<i64>> {
        tokio::task::yield_now().await; // a real backlog read yields: no hang on a looping sweep
        Ok(Vec::new())
    }
}

/// Spawn the fixture worker (plain child, piped stdio) through the PRODUCTION
/// stack: PersistentWorker (supervision) + PolledWorkerDriver (poll/identity/
/// pending) + MatrixChannel — everything but the sandbox and the sidecar.
fn spawn_matrix_channel(sent_file: &Path, peer: &str) -> Option<MatrixChannel> {
    spawn_matrix_channel_with(sent_file, peer, 0, DriverAudit::none())
}

/// [`spawn_matrix_channel`], with the fixture holding `held` events at drain
/// time (`FAKE_MATRIX_HELD`) and the driver given `audit`.
fn spawn_matrix_channel_with(
    sent_file: &Path,
    peer: &str,
    held: usize,
    audit: DriverAudit,
) -> Option<MatrixChannel> {
    let bin = fixture_bin();
    if !bin.exists() {
        eprintln!(
            "\n[SKIP] fixture not built: {} — run `cargo build -p kastellan-worker-matrix --examples`\n",
            bin.display()
        );
        return None;
    }
    let sent_file = sent_file.to_path_buf();
    let peer = peer.to_string();
    let factory: PersistentFactory = Box::new(move || {
        let child = Command::new(&bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("FAKE_MATRIX_SENT", &sent_file)
            .env("FAKE_MATRIX_PEER", &peer)
            .env("FAKE_MATRIX_ROOM", "!room:srv")
            .env("FAKE_MATRIX_BODY", "hello from peer")
            .env("FAKE_MATRIX_HELD", held.to_string())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn fake matrix worker: {e}"))?;
        let client = Client::from_child(child)
            .map_err(|e| anyhow::anyhow!("connect to fake worker: {e}"))?;
        Ok(Box::new(ClientTransport::from_client(client)) as Box<dyn PersistentTransport>)
    });
    let handle = PersistentWorker::spawn("matrix-e2e", factory).expect("persistent spawn");
    let (driver, identity) = PolledWorkerDriver::spawn(
        MATRIX_POLLED_SPEC,
        Box::new(handle),
        parse_matrix_poll,
        encode_matrix_send,
        None, // Matrix has no ack cursor — MATRIX_POLLED_SPEC.ack_method is None too.
        None, // No skipped-id extraction either: no ack cursor, so nothing to ack past. What parse_matrix_poll cannot decode it counts itself (#852).
        audit, // No database here: a test that needs a hook passes a recorder.
        ChannelId("matrix".into()),
    )
    .expect("polled driver spawn");
    assert_eq!(identity["user_id"], "@bot:srv", "matrix.init identity must surface");
    Some(MatrixChannel::from_driver(ChannelId("matrix".into()), driver))
}

#[tokio::test]
async fn paired_inbound_round_trips_to_a_sent_reply() {
    let sent_file = std::env::temp_dir().join(format!("kastellan-matrix-sent-{}", std::process::id()));
    let _ = std::fs::remove_file(&sent_file);

    let Some(ch) = spawn_matrix_channel(&sent_file, "@me:srv") else { return };

    let events = FakeEvents::default();
    let completed = FakeCompleted {
        enqueued: events.enqueued.clone(),
        yielded: std::sync::atomic::AtomicBool::new(false),
    };
    let bus = ChannelBus::spawn(
        vec![Box::new(ch)],
        Arc::new(StaticPairings::from_peers([PeerId("@me:srv".into())])),
        None,
        Arc::new(events),
        Arc::new(completed),
        None,
    );

    // The fixture's canned inbound is paired + benign → enqueued → completed →
    // routed reply "pong" → fixture appends it to sent_file. Poll until a
    // COMPLETE line lands: the fixture's writeln! is not atomic, so a reader
    // polling on "non-empty" can catch a half-written JSON line (a real flake
    // seen in CI) — wait for the terminating newline instead.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while std::time::Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(&sent_file) {
            if s.contains('\n') {
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
    let completed = FakeCompleted {
        enqueued: events.enqueued.clone(),
        yielded: std::sync::atomic::AtomicBool::new(false),
    };
    let bus = ChannelBus::spawn(
        vec![Box::new(ch)],
        Arc::new(StaticPairings::from_peers([PeerId("@me:srv".into())])), // stranger not paired
        None,
        Arc::new(events.clone()),
        Arc::new(completed),
        None,
    );

    // Give the loop time; the unpaired message must NOT enqueue and NOTHING is sent.
    tokio::time::sleep(Duration::from_millis(400)).await;
    bus.shutdown().await;

    assert!(events.enqueued.lock().unwrap().is_empty(), "unpaired peer must not enqueue a task");
    let sent = std::fs::read_to_string(&sent_file).unwrap_or_default();
    assert!(sent.trim().is_empty(), "no reply should be sent for an unpaired peer");
    let _ = std::fs::remove_file(&sent_file);
}

/// #835 through a real worker process: the fixture says it still held 2
/// events when the channel stopped, and the driver's last call — the real
/// `matrix.drain` over real pipes, decoded by `parse_matrix_drain` — hands
/// that count to the inbound-dropped hook at the `worker` stage.
#[tokio::test]
async fn events_the_worker_still_held_at_stop_reach_the_audit_hook() {
    let sent_file = std::env::temp_dir().join(format!("kastellan-matrix-held-{}", std::process::id()));
    let _ = std::fs::remove_file(&sent_file);
    let dropped: Arc<Mutex<Vec<(InboundDropStage, usize)>>> = Arc::default();
    let sink = Arc::clone(&dropped);
    let audit = DriverAudit {
        inbound_dropped: Some(Arc::new(move |d: InboundDropped<'_>| {
            sink.lock().unwrap().push((d.stage, d.dropped.get()))
        })),
        ..DriverAudit::none()
    };
    let Some(ch) = spawn_matrix_channel_with(&sent_file, "@stranger:srv", 2, audit) else { return };

    // Dropping the channel stops the driver (detached thread): wait for its
    // last report rather than for the thread.
    drop(ch);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if dropped.lock().unwrap().iter().any(|(s, _)| *s == InboundDropStage::Worker) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Settle, so a duplicate report arriving after the first is seen too.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = std::fs::remove_file(&sent_file);
    let worker: Vec<_> =
        dropped.lock().unwrap().iter().filter(|(s, _)| *s == InboundDropStage::Worker).copied().collect();
    assert_eq!(worker, [(InboundDropStage::Worker, 2)], "all hook calls: {:?}", dropped.lock().unwrap());
}
